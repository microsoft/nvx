//! Cross-process exec admission through the actual CLI and fake NVXC server.

use std::fs::{self, File, TryLockError};
use std::process::{Child, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::harness::{LAYER, Outcome, Root};
use crate::lifecycle::{provision, sandbox};

struct Pending {
    child: Option<Child>,
    arguments: Vec<String>,
}

impl Pending {
    fn spawn(root: &Root, options: &[String]) -> Self {
        let arguments = options
            .iter()
            .map(|value| root.expand(value))
            .collect::<Vec<_>>();
        let child = root
            .command(&arguments)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Self {
            child: Some(child),
            arguments,
        }
    }

    fn finish(mut self) -> Outcome {
        let output = self.child.take().unwrap().wait_with_output().unwrap();
        Outcome::new(self.arguments.clone(), output)
    }

    fn ended(&mut self) -> bool {
        self.child.as_mut().unwrap().try_wait().unwrap().is_some()
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn start(root: &Root, state: &str) {
    root.nvx_rs(&sandbox(
        "provision",
        &format!("{{root/{state}}}"),
        &["--layer", LAYER, "--upper", "ram"],
    ))
    .assert_silent_success();
    operate(root, "start", state, &[]).assert_silent_success();
}

fn operate(root: &Root, operation: &str, state: &str, options: &[&str]) -> Outcome {
    root.nvx_rs(&sandbox(operation, &format!("{{root/{state}}}"), options))
}

fn script(root: &Root, state: &str, script: &str, options: &[&str]) -> Vec<String> {
    let mut arguments = sandbox(
        "exec",
        &format!("{{root/{state}}}"),
        &["--entrypoint", "/bin/sh", "--arg=-c", "--arg", script],
    );
    arguments.extend(options.iter().map(|value| (*value).to_owned()));
    arguments.iter().map(|value| root.expand(value)).collect()
}

#[test]
fn exec_processes_contend_but_other_sandboxes_and_stop_proceed() {
    let root = Root::new();
    start(&root, "first");
    start(&root, "second");
    let first = Pending::spawn(
        &root,
        &script(&root, "first", "touch {root/entered}; sleep 60000", &[]),
    );
    crate::wait_for(&root.below("entered"));
    let admission = File::options()
        .read(true)
        .write(true)
        .open(root.below("first/exec.lock"))
        .unwrap();
    assert!(matches!(
        admission.try_lock(),
        Err(TryLockError::WouldBlock)
    ));
    let sessions = crate::sessions(&root, "first");
    let started = Instant::now();
    let refused = root.nvx_rs(&script(
        &root,
        "first",
        "touch {root/forbidden}",
        &["--wait", "0.05"],
    ));
    refused.assert_error("sandbox-busy", &["another exec"]);
    // The refusal came after `--wait`, and long before the 60-second default, however slowly a
    // loaded host starts the process.
    assert!(started.elapsed() >= Duration::from_millis(50));
    assert!(started.elapsed() < Duration::from_secs(30));
    assert!(!root.below("forbidden").exists());
    assert_eq!(crate::sessions(&root, "first"), sessions);
    operate(
        &root,
        "exec",
        "second",
        &[
            "--entrypoint",
            "/bin/echo",
            "--arg",
            "independent",
            "--wait",
            "0.05",
        ],
    )
    .assert_success();
    assert!(matches!(
        admission.try_lock(),
        Err(TryLockError::WouldBlock)
    ));
    let process = crate::recorded_process(&root, "first").unwrap();
    let stop_started = Instant::now();
    // The fake serves one control session at a time, so the exec's session keeps the stop's
    // request from the guest, and the stop forces the VM once its grace of `--timeout` has passed.
    // It then waits up to `--timeout` again for the terminated OpenVMM to exit, which a loaded
    // Windows host can take over a second to confirm.
    let stopped = operate(&root, "stop", "first", &["--timeout", "5"]);
    assert_eq!(stopped.status, Some(0), "{}", stopped.describe());
    assert!(stopped.stdout().is_empty(), "{}", stopped.describe());
    assert!(
        stopped
            .stderr()
            .lines()
            .all(|line| line.starts_with("warning: ")),
        "{}",
        stopped.describe()
    );
    assert!(
        stopped.stderr().contains("forcibly terminated"),
        "{}",
        stopped.describe()
    );
    // The stop waited neither for the exec lock nor for the workload, which sleeps for a minute.
    assert!(stop_started.elapsed() < Duration::from_secs(30));
    assert!(
        process
            .wait(Instant::now() + Duration::from_secs(10))
            .unwrap()
            .is_some()
    );
    let ended = first.finish();
    ended.assert_error("endpoint-closed", &[]);
    admission.try_lock().unwrap();
    admission.unlock().unwrap();
    drop(admission);
    operate(&root, "deprovision", "first", &[]).assert_silent_success();
    assert!(!root.below("first").exists());
    start(&root, "first");
    operate(
        &root,
        "exec",
        "first",
        &[
            "--entrypoint",
            "/bin/echo",
            "--arg",
            "new",
            "--wait",
            "0.05",
        ],
    )
    .assert_success();
    for state in ["first", "second"] {
        operate(&root, "stop", state, &[]).assert_silent_success();
        operate(&root, "deprovision", state, &[]).assert_silent_success();
    }
}

#[test]
fn a_retry_that_waits_for_the_lifecycle_lock_past_the_deadline_is_busy() {
    // The `--wait` of the waiting exec, whose first half leaves a loaded host the seconds that it
    // can take to start the exec and to let the test find one of its tries.
    const WAIT: Duration = Duration::from_secs(8);
    let root = Root::new();
    start(&root, "state");
    operate(
        &root,
        "exec",
        "state",
        &["--entrypoint", "/bin/echo", "--arg", "prepared"],
    )
    .assert_success();
    // The test holds the exec lock, as an exec in progress does.
    let admission = File::options()
        .read(true)
        .write(true)
        .open(root.below("state/exec.lock"))
        .unwrap();
    admission.lock().unwrap();
    let lifecycle = File::options()
        .read(true)
        .write(true)
        .open(root.below("state/sandbox.lock"))
        .unwrap();
    let wait = WAIT.as_secs().to_string();
    // The race needs an exec whose retry waits for the lifecycle lock until after its deadline. The
    // exec holds the lifecycle lock only while it tries the exec lock, so the test polls that lock
    // until it finds a try, and then takes it. It polls without yielding: on a loaded host, a yield
    // lets every other thread that waits for the processor run first, while the tries come and go.
    // When it takes the lock within the first half of `--wait`, the deadline, at least `--wait`
    // after the spawn, is still half of it away, so the exec tries again and waits for the
    // lifecycle lock: the precondition holds, and the round must end in `sandbox-busy` once the
    // test frees the exec lock and then, past the deadline, the lifecycle lock. Only a round that
    // misses every try, or finds one too late, gives way to another, once its exec has waited out
    // `--wait` and failed with `sandbox-busy` too, and the test fails if no round reaches the
    // precondition.
    let reached = (0..5).any(|_| {
        let spawned = Instant::now();
        let mut waiting = Pending::spawn(
            &root,
            &script(&root, "state", "touch {root/forbidden}", &["--wait", &wait]),
        );
        let seen = loop {
            match lifecycle.try_lock() {
                Ok(()) => lifecycle.unlock().unwrap(),
                Err(TryLockError::WouldBlock) => break true,
                Err(TryLockError::Error(error)) => panic!("cannot try the lock: {error}"),
            }
            if waiting.ended() {
                break false;
            }
            assert!(spawned.elapsed() < Duration::from_secs(60), "the exec hung");
        };
        let reached = seen && {
            lifecycle.lock().unwrap();
            spawned.elapsed() < WAIT / 2
        };
        if reached {
            // The deadline, which the exec took after the spawn and before its try, passes.
            thread::sleep(WAIT + Duration::from_millis(500));
            assert!(
                !waiting.ended(),
                "the exec ended instead of waiting to try again"
            );
            admission.unlock().unwrap();
            lifecycle.unlock().unwrap();
        } else if seen {
            lifecycle.unlock().unwrap();
        }
        waiting
            .finish()
            .assert_error("sandbox-busy", &["another exec"]);
        reached
    });
    assert!(
        reached,
        "no round delayed a retry of the exec past its deadline"
    );
    assert!(!root.below("forbidden").exists());
    drop((admission, lifecycle));
    operate(
        &root,
        "exec",
        "state",
        &[
            "--entrypoint",
            "/bin/echo",
            "--arg",
            "free",
            "--wait",
            "0.05",
        ],
    )
    .assert_success();
    operate(&root, "stop", "state", &[]).assert_silent_success();
    operate(&root, "deprovision", "state", &[]).assert_silent_success();
}

#[test]
fn results_errors_and_process_cancellation_release_admission() {
    let root = Root::new();
    root.nvx_rs(&provision("{root/state}", &[]))
        .assert_silent_success();
    operate(&root, "start", "state", &[]).assert_silent_success();
    for (command, status) in [
        ("echo success", 0),
        ("exit 3", 3),
        ("fail", 125),
        ("launchfail", 1),
    ] {
        let outcome = root.nvx_rs(&script(&root, "state", command, &["--wait", "0.05"]));
        assert_eq!(outcome.status, Some(status), "{}", outcome.describe());
        operate(
            &root,
            "exec",
            "state",
            &[
                "--entrypoint",
                "/bin/echo",
                "--arg",
                "released",
                "--wait",
                "0.05",
            ],
        )
        .assert_success();
    }
    let mut pending = Pending::spawn(
        &root,
        &script(&root, "state", "touch {root/cancelling}; sleep 60000", &[]),
    );
    crate::wait_for(&root.below("cancelling"));
    #[cfg(unix)]
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(pending.child.as_mut().unwrap().id()).unwrap()),
        nix::sys::signal::Signal::SIGINT,
    )
    .unwrap();
    #[cfg(windows)]
    pending.child.as_mut().unwrap().kill().unwrap();
    let cancelled = pending.finish();
    #[cfg(unix)]
    assert_eq!(cancelled.status, Some(130), "{}", cancelled.describe());
    #[cfg(windows)]
    assert_ne!(cancelled.status, Some(0), "{}", cancelled.describe());
    operate(
        &root,
        "exec",
        "state",
        &[
            "--entrypoint",
            "/bin/echo",
            "--arg",
            "after-cancel",
            "--wait",
            "1",
        ],
    )
    .assert_success();
    operate(&root, "stop", "state", &[]).assert_silent_success();
    operate(&root, "deprovision", "state", &[]).assert_silent_success();
}

#[test]
fn exec_cannot_recreate_admission_after_cleanup_removes_provisioned_state() {
    let root = Root::new();
    start(&root, "state");
    operate(
        &root,
        "exec",
        "state",
        &["--entrypoint", "/bin/echo", "--arg", "owned"],
    )
    .assert_success();
    let process = crate::recorded_process(&root, "state").unwrap();
    operate(&root, "stop", "state", &[]).assert_silent_success();
    assert!(
        process
            .wait(Instant::now() + Duration::from_secs(10))
            .unwrap()
            .is_some()
    );
    let lifecycle = File::options()
        .read(true)
        .write(true)
        .open(root.below("state/sandbox.lock"))
        .unwrap();
    lifecycle.lock().unwrap();
    root.remove("state/config.json");
    root.remove("state/exec.lock");
    let refused = operate(
        &root,
        "exec",
        "state",
        &["--wait", "0.05", "--timeout", "0.05"],
    );
    refused.assert_error("sandbox-busy", &["lifecycle"]);
    assert!(!root.below("state/exec.lock").exists());
    lifecycle.unlock().unwrap();
    drop(lifecycle);
    let refused = operate(&root, "exec", "state", &["--wait", "0.05"]);
    refused.assert_error("sandbox-not-provisioned", &[]);
    assert!(!root.below("state/exec.lock").exists());
    operate(&root, "deprovision", "state", &[]).assert_silent_success();
    assert!(!root.below("state").exists());
    start(&root, "state");
    operate(
        &root,
        "exec",
        "state",
        &["--entrypoint", "/bin/echo", "--arg", "reprovisioned"],
    )
    .assert_success();
    operate(&root, "stop", "state", &[]).assert_silent_success();
    operate(&root, "deprovision", "state", &[]).assert_silent_success();
}

#[cfg(windows)]
#[test]
fn deprovision_waits_for_its_delete_pending_exec_file_after_verified_exit() {
    let root = Root::new();
    start(&root, "state");
    operate(
        &root,
        "exec",
        "state",
        &["--entrypoint", "/bin/echo", "--arg", "owned"],
    )
    .assert_success();
    let process = crate::recorded_process(&root, "state").unwrap();
    operate(&root, "stop", "state", &[]).assert_silent_success();
    assert!(
        process
            .wait(Instant::now() + Duration::from_secs(10))
            .unwrap()
            .is_some()
    );
    let state = root.below("state");
    let pending = crate::lifecycle::windows::delete_pending(&state.join("exec.lock"));
    assert!(
        fs::read_dir(&state)
            .unwrap()
            .any(|entry| entry.unwrap().file_name() == "exec.lock")
    );
    let mut deprovision = Pending::spawn(
        &root,
        &sandbox("deprovision", "{root/state}", &["--timeout", "2"]),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while state.join("config.json").exists() {
        assert!(
            Instant::now() < deadline,
            "deprovision did not reach cleanup"
        );
        assert!(
            deprovision
                .child
                .as_mut()
                .unwrap()
                .try_wait()
                .unwrap()
                .is_none()
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        deprovision
            .child
            .as_mut()
            .unwrap()
            .try_wait()
            .unwrap()
            .is_none()
    );
    assert!(
        fs::read_dir(&state)
            .unwrap()
            .any(|entry| entry.unwrap().file_name() == "exec.lock")
    );
    drop(pending);
    deprovision.finish().assert_silent_success();
    assert!(!state.exists());
}

#[test]
fn a_replaced_exec_lock_entry_is_refused_before_contacting_the_vm() {
    let root = Root::new();
    start(&root, "state");
    operate(
        &root,
        "exec",
        "state",
        &["--entrypoint", "/bin/echo", "--arg", "prepared"],
    )
    .assert_success();
    fs::rename(root.below("state/exec.lock"), root.below("state/old.lock")).unwrap();
    fs::create_dir(root.below("state/exec.lock")).unwrap();
    let sessions = crate::sessions(&root, "state");
    let refused = root.nvx_rs(&script(
        &root,
        "state",
        "touch {root/forbidden}",
        &["--wait", "0.05"],
    ));
    refused.assert_error("io", &["exec.lock"]);
    assert!(!root.below("forbidden").exists());
    assert_eq!(crate::sessions(&root, "state"), sessions);
    fs::remove_dir(root.below("state/exec.lock")).unwrap();
    fs::rename(root.below("state/old.lock"), root.below("state/exec.lock")).unwrap();
    operate(
        &root,
        "exec",
        "state",
        &[
            "--entrypoint",
            "/bin/echo",
            "--arg",
            "current",
            "--wait",
            "0.05",
        ],
    )
    .assert_success();
    operate(&root, "stop", "state", &[]).assert_silent_success();
    operate(&root, "deprovision", "state", &[]).assert_silent_success();
}
