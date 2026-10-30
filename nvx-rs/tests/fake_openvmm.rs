//! Drives VM instances end to end against the `nvx-fake-openvmm` test double, which serves the
//! real control protocol without running a VM.
//!
//! An orchestrator that starts an instance in a process of its own, which then exits, lets a test
//! restart the orchestrator within the same host boot.
#![cfg(all(
    feature = "host",
    feature = "testing",
    any(target_os = "linux", windows)
))]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use nvx::host::instance::{Found, Instance, Running, Started, Stopped, WorkloadOutcome};
use nvx::host::launch::{LaunchSpec, PROCESS_COMMAND_LINE_LIMIT};
use nvx::host::process::{Process, ProcessRef};
use nvx::host::sandbox::mount::aggregate_child_name;
use nvx::host::store::{ArtifactIdentity, CallerFiles, IdentityPart, RuntimeIdentity};
use nvx::host::supervisor::spawn_command;
use nvx::protocol::{Workload, WorkloadEnvironment};
use nvx::{Artifacts, ErrorKind, Hypervisor};
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(30);
/// The instance directory in which [`orchestrator_role`] starts a VM.
const ORCHESTRATOR_INSTANCE: &str = "NVX_FAKE_OPENVMM_TEST_INSTANCE";

/// Returns the runtime identity of these tests' launches, with the OpenVMM identity of the digit
/// `openvmm`.
fn runtime_with(openvmm: char) -> RuntimeIdentity {
    let artifact = |digit: char| ArtifactIdentity::sha256(&digit.to_string().repeat(64)).unwrap();
    RuntimeIdentity::new(artifact(openvmm), artifact('2'), artifact('3'))
}

/// Returns the runtime identity of these tests' launches.
fn runtime() -> RuntimeIdentity {
    runtime_with('1')
}

/// Creates the fake guest artifacts in `directory`, and returns the specification of a VM that
/// runs them.
fn spec(directory: &Path) -> LaunchSpec {
    fs::write(directory.join("vmlinux"), b"kernel").unwrap();
    fs::write(directory.join("initramfs.cpio.gz"), b"initrd").unwrap();
    let artifacts = Artifacts::new(
        env!("CARGO_BIN_EXE_nvx-fake-openvmm"),
        directory.join("vmlinux"),
        directory.join("initramfs.cpio.gz"),
    );
    let hypervisor = Hypervisor::platform_default().expect("tests run on Linux or Windows");
    LaunchSpec::new(artifacts, hypervisor)
}

/// Creates the fake guest artifacts and an instance directory below `directory`.
fn setup(directory: &Path) -> (LaunchSpec, Instance) {
    let instance = Instance::create(directory.join("vm"), "test", CallerFiles::none()).unwrap();
    (spec(directory), instance)
}

/// Finds the instance's VM under these tests' runtime identity.
fn find(instance: &Instance) -> Found {
    instance.lock(None).unwrap().find(|| Ok(runtime())).unwrap()
}

/// Returns the instance's running VM.
fn running(instance: &Instance) -> Running {
    match find(instance) {
        Found::Running(running) => running,
        other => panic!("the instance does not run: {other:?}"),
    }
}

/// Starts the instance's VM, which does not run, with `timeout`.
fn start_within(instance: &Instance, spec: &LaunchSpec, timeout: Duration) -> nvx::Result<Started> {
    let mut locked = instance.lock(None).unwrap();
    assert!(matches!(locked.find(|| Ok(runtime()))?, Found::Stopped));
    locked.start(spec, &runtime(), timeout)
}

/// Starts the instance's VM, which does not run.
fn start(instance: &Instance, spec: &LaunchSpec) -> nvx::Result<Started> {
    start_within(instance, spec, TIMEOUT)
}

/// Stops the instance's running VM with `timeout`.
fn stop(instance: &Instance, timeout: Duration) -> nvx::Result<Stopped> {
    let mut locked = instance.lock(None).unwrap();
    let Found::Running(running) = locked.find(|| Ok(runtime()))? else {
        panic!("the instance does not run");
    };
    locked.stop(running, timeout)
}

fn workload(argv: &[&str]) -> Workload<'static> {
    Workload {
        argv: argv.iter().map(|argument| (*argument).to_owned()).collect(),
        timeout_ms: 0,
        cwd: None,
        environment: WorkloadEnvironment::Default,
    }
}

struct Output {
    outcome: WorkloadOutcome,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Runs `workload` in the instance's running VM, which it finds as every exec does.
fn run(instance: &Instance, workload: &Workload<'_>, cancel: &AtomicBool) -> Output {
    let running = running(instance);
    let execution = instance.exec(&running, workload, TIMEOUT, TIMEOUT).unwrap();
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let outcome = execution
        .pump(
            &mut |chunk: &[u8]| {
                stdout.extend_from_slice(chunk);
                Ok(())
            },
            &mut |chunk: &[u8]| {
                stderr.extend_from_slice(chunk);
                Ok(())
            },
            cancel,
        )
        .unwrap();
    Output {
        outcome,
        stdout,
        stderr,
    }
}

/// Returns whether the store of `instance` records neither a launch nor a VM.
fn records_nothing(instance: &Instance) -> bool {
    let store = instance.store();
    store.journal().unwrap().is_none()
        && store.runtime().unwrap().is_none()
        && store.read_capability().is_err()
}

#[test]
fn instances_start_run_workloads_and_stop() {
    let directory = tempfile::tempdir().unwrap();
    let (spec, instance) = setup(directory.path());
    assert!(matches!(find(&instance), Found::Stopped));
    let started = start(&instance, &spec).unwrap();
    assert_eq!(started.runtime.runtime(), &runtime());
    assert_eq!(running(&instance).runtime(), &started.runtime);
    // Only a launch in progress keeps a journal.
    assert!(instance.store().journal().unwrap().is_none());

    let idle = AtomicBool::new(false);
    let output = run(
        &instance,
        &workload(&["/bin/sh", "-c", "echo hello; echoerr warning; exit 3"]),
        &idle,
    );
    assert_eq!(output.outcome, WorkloadOutcome::Exited(3));
    assert_eq!(output.stdout, b"hello\n");
    assert_eq!(output.stderr, b"warning\n");
    let signaled = run(&instance, &workload(&["/bin/sh", "-c", "signal 15"]), &idle);
    assert_eq!(signaled.outcome, WorkloadOutcome::Signaled(15));
    let mut timed = workload(&["/bin/sh", "-c", "sleep 10000"]);
    timed.timeout_ms = 50;
    assert_eq!(
        run(&instance, &timed, &idle).outcome,
        WorkloadOutcome::TimedOut
    );
    let mut missing = workload(&["/bin/pwd"]);
    missing.cwd = Some("/missing");
    let refused = run(&instance, &missing, &idle);
    assert_eq!(refused.outcome, WorkloadOutcome::WorkingDirectory);
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("/missing"),
        "{:?}",
        refused.stderr
    );

    let stopped = stop(&instance, TIMEOUT).unwrap();
    assert!(!stopped.forced);
    let report = stopped.report.expect("the VM reports its outcome");
    assert_eq!(report.category.as_deref(), Some("success"));
    assert_eq!(report.teardown_complete, Some(true));
    assert!(matches!(find(&instance), Found::Stopped));
    assert!(records_nothing(&instance));
    // The log and the report outlive the run.
    assert!(instance.log_path().is_file());
    assert!(instance.report_path().is_file());
}

#[test]
fn timeouts_are_checked_before_they_are_used() {
    let directory = tempfile::tempdir().unwrap();
    let (spec, instance) = setup(directory.path());
    for timeout in [Duration::ZERO, Duration::MAX] {
        let error = start_within(&instance, &spec, timeout).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        // Nothing was launched.
        assert!(records_nothing(&instance));
    }
    start(&instance, &spec).unwrap();
    let echo = workload(&["/bin/echo", "hello"]);
    let vm = running(&instance);
    for (control_timeout, response_grace) in [
        (Duration::ZERO, TIMEOUT),
        (Duration::MAX, TIMEOUT),
        (TIMEOUT, Duration::MAX),
    ] {
        let Err(error) = instance.exec(&vm, &echo, control_timeout, response_grace) else {
            panic!("exec accepted {control_timeout:?} and {response_grace:?}");
        };
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
    }
    drop(vm);
    for timeout in [Duration::ZERO, Duration::MAX] {
        let error = stop(&instance, timeout).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
    }
    // The refused requests left the VM running.
    let idle = AtomicBool::new(false);
    let output = run(&instance, &echo, &idle);
    assert_eq!(output.outcome, WorkloadOutcome::Exited(0));
    assert_eq!(output.stdout, b"hello\n");
    stop(&instance, TIMEOUT).unwrap();
}

#[test]
fn launches_beyond_the_process_command_line_limit_are_refused() {
    let directory = tempfile::tempdir().unwrap();
    let (mut spec, instance) = setup(directory.path());
    // The log of an earlier start outlives a launch that is refused.
    start(&instance, &spec).unwrap();
    stop(&instance, TIMEOUT).unwrap();
    let log = fs::read(instance.log_path()).unwrap();
    // The program's path counts toward the command line, which every host holds to the limit.
    spec.openvmm = directory
        .path()
        .join("o".repeat(PROCESS_COMMAND_LINE_LIMIT));
    let error = start(&instance, &spec).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
    // Nothing was launched or recorded.
    assert!(records_nothing(&instance));
    assert_eq!(fs::read(instance.log_path()).unwrap(), log);
}

#[test]
fn cancellation_ends_a_running_workload() {
    let directory = tempfile::tempdir().unwrap();
    let (spec, instance) = setup(directory.path());
    start(&instance, &spec).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let canceller = thread::spawn({
        let cancel = Arc::clone(&cancel);
        move || {
            thread::sleep(Duration::from_millis(200));
            cancel.store(true, Ordering::Release);
        }
    });
    let output = run(
        &instance,
        &workload(&["/bin/sh", "-c", "echo started; sleep 60000"]),
        &cancel,
    );
    canceller.join().unwrap();
    assert_eq!(output.outcome, WorkloadOutcome::Cancelled);
    assert_eq!(output.stdout, b"started\n");
    stop(&instance, TIMEOUT).unwrap();
}

#[test]
fn unresponsive_guests_are_terminated() {
    let directory = tempfile::tempdir().unwrap();
    let (mut spec, instance) = setup(directory.path());
    spec.kernel_parameters = "fake_ignore_stop=1".to_owned();
    start(&instance, &spec).unwrap();
    let stopped = stop(&instance, Duration::from_millis(500)).unwrap();
    assert!(stopped.forced);
    assert!(matches!(find(&instance), Found::Stopped));
    assert!(records_nothing(&instance));
}

#[test]
fn a_vm_whose_capability_is_lost_is_still_stopped() {
    // Only control sessions need the capability: without it, exec fails as reading it fails, and
    // stop ends the VM through its verified process identity.
    let directory = tempfile::tempdir().unwrap();
    let (spec, instance) = setup(directory.path());
    let capability = instance.store().capability_path();
    for damage in ["missing", "truncated"] {
        start(&instance, &spec).unwrap();
        match damage {
            "missing" => fs::remove_file(&capability).unwrap(),
            _ => fs::write(&capability, b"short").unwrap(),
        }
        let running = running(&instance);
        let Err(error) = instance.exec(&running, &workload(&["/bin/true"]), TIMEOUT, TIMEOUT)
        else {
            panic!("{damage}: exec attached without the capability");
        };
        assert_eq!(error.kind(), ErrorKind::Io, "{damage}: {error}");
        assert!(
            error.message().contains("sandbox control capability"),
            "{damage}: {error}"
        );
        drop(running);
        let stopped = stop(&instance, TIMEOUT).unwrap();
        assert!(stopped.forced, "{damage}");
        assert!(matches!(find(&instance), Found::Stopped), "{damage}");
        assert!(records_nothing(&instance), "{damage}");
        assert!(!capability.exists(), "{damage}");
    }
}

#[test]
fn guests_without_required_features_are_refused() {
    let directory = tempfile::tempdir().unwrap();
    let (mut spec, instance) = setup(directory.path());
    // Cancellation alone.
    spec.kernel_parameters = "fake_guest_features=1".to_owned();
    let error = start(&instance, &spec).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable);
    assert!(
        error.message().contains("lacks control features"),
        "{error}"
    );
    assert!(error.message().contains("sandbox test failed to start"));
    // The rejected VM is gone and leaves no runtime state behind.
    assert!(matches!(find(&instance), Found::Stopped));
    assert!(records_nothing(&instance));
}

#[test]
fn launches_that_exit_fail_to_start() {
    let directory = tempfile::tempdir().unwrap();
    let (mut spec, instance) = setup(directory.path());
    spec.kernel_parameters = "fake_exit_on_start=4".to_owned();
    let error = start(&instance, &spec).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Exited, "{error}");
    assert!(
        error
            .message()
            .contains(&instance.log_path().display().to_string()),
        "{error}"
    );
    assert!(matches!(find(&instance), Found::Stopped));
    assert!(records_nothing(&instance));
}

/// The orchestrator that [`a_restarted_orchestrator_reattaches_to_its_vm`] runs in a process of
/// its own: it starts the instance in the directory that [`ORCHESTRATOR_INSTANCE`] names, with the
/// fake guest artifacts of its working directory, and exits, leaving the VM running.
#[test]
#[ignore = "an orchestrator that the tests run in a process of its own"]
fn orchestrator_role() {
    let Some(dir) = env::var_os(ORCHESTRATOR_INSTANCE) else {
        return;
    };
    let spec = spec(&env::current_dir().unwrap());
    let instance = Instance::create(PathBuf::from(dir), "test", CallerFiles::none()).unwrap();
    start(&instance, &spec).unwrap();
}

#[test]
fn a_restarted_orchestrator_reattaches_to_its_vm() {
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path().join("vm");
    let mut command = Command::new(env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "orchestrator_role",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .current_dir(directory.path())
        .env(ORCHESTRATOR_INSTANCE, &dir)
        .stdin(Stdio::null());
    let status = spawn_command(&mut command).unwrap().wait().unwrap();
    assert!(status.success(), "the orchestrator failed: {status}");

    // The orchestrator restarts within the same host boot: it finds the VM that its earlier run
    // started, through the records of the store, and drives it.
    let instance = Instance::open(&dir, "test", CallerFiles::none()).unwrap();
    let idle = AtomicBool::new(false);
    let output = run(&instance, &workload(&["/bin/echo", "reattached"]), &idle);
    assert_eq!(output.outcome, WorkloadOutcome::Exited(0));
    assert_eq!(output.stdout, b"reattached\n");

    // Under another runtime identity, as after an upgrade of OpenVMM, the VM is mismatched: it is
    // reported, and not driven.
    {
        let mut locked = instance.lock(None).unwrap();
        let Found::Mismatched(mismatched) = locked.find(|| Ok(runtime_with('9'))).unwrap() else {
            panic!("a VM of another runtime identity was not reported");
        };
        assert_eq!(mismatched.differences(), [IdentityPart::OpenVmm]);
        let error = mismatched.error();
        assert_eq!(
            error.message(),
            format!(
                "sandbox test runs under another runtime identity, which differs in OpenVMM, so \
                 it is not driven: stop it to terminate its OpenVMM process {} without a guest \
                 shutdown",
                mismatched.vmm().record().process().pid
            )
        );
    }
    assert_eq!(
        run(&instance, &workload(&["/bin/echo", "still"]), &idle).stdout,
        b"still\n"
    );

    // Its stop terminates OpenVMM through its verified process identity, and opens no control
    // session, whose protocol the guest might not speak.
    let sessions = sessions(directory.path());
    assert!(sessions > 0, "the fake OpenVMM recorded no session");
    let mut locked = instance.lock(None).unwrap();
    let Found::Mismatched(mismatched) = locked.find(|| Ok(runtime_with('9'))).unwrap() else {
        panic!("a VM of another runtime identity was not reported");
    };
    let process = Process::open(mismatched.vmm().record().process())
        .unwrap()
        .expect("the VM runs");
    for timeout in [Duration::ZERO, Duration::MAX] {
        let Found::Mismatched(mismatched) = locked.find(|| Ok(runtime_with('9'))).unwrap() else {
            panic!("a VM of another runtime identity was not reported");
        };
        let error = locked.terminate(mismatched, timeout).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
    }
    assert!(!process.has_exited().unwrap());
    let stopped = locked.terminate(mismatched, TIMEOUT).unwrap();
    assert!(stopped.forced);
    // A terminated OpenVMM reports no outcome.
    assert!(stopped.report.is_none());
    assert!(process.has_exited().unwrap());
    assert_eq!(self::sessions(directory.path()), sessions);
    drop(locked);
    assert!(matches!(find(&instance), Found::Stopped));
    assert!(records_nothing(&instance));
}

/// Returns how many connections to its control endpoint the fake OpenVMM of the instance `vm`,
/// whose guest artifacts are in `directory`, has accepted.
fn sessions(directory: &Path) -> usize {
    fs::read_to_string(directory.join("fake-openvmm-vm-sessions.log"))
        .map_or(0, |log| log.lines().count())
}

#[test]
fn outcome_reports_of_another_runtime_are_never_read() {
    let directory = tempfile::tempdir().unwrap();
    let (spec, instance) = setup(directory.path());
    // A report beside a VM of another runtime identity is that runtime's, under its contracts, so
    // its stop reads none: neither when it terminates OpenVMM nor when OpenVMM exited, and
    // published its report, after the VM was found.
    for exits_first in [false, true] {
        start(&instance, &spec).unwrap();
        let mut locked = instance.lock(None).unwrap();
        let Found::Mismatched(mismatched) = locked.find(|| Ok(runtime_with('9'))).unwrap() else {
            panic!("a VM of another runtime identity was not reported");
        };
        let process = Process::open(mismatched.vmm().record().process())
            .unwrap()
            .expect("the VM runs");
        fs::write(
            instance.report_path(),
            r#"{"schema_version":1,"outcome":{"operation":"managed","category":"success",
               "status_code":0},"teardown":{"workers":true}}"#,
        )
        .unwrap();
        if exits_first {
            process.kill().unwrap();
            assert!(process.wait(Instant::now() + TIMEOUT).unwrap());
        }
        let stopped = locked.terminate(mismatched, TIMEOUT).unwrap();
        assert_eq!(
            stopped,
            Stopped {
                forced: !exits_first,
                report: None,
            }
        );
        assert!(process.has_exited().unwrap());
        drop(locked);
        assert!(records_nothing(&instance));
    }
}

#[test]
fn records_that_name_another_process_never_end_it() {
    let directory = tempfile::tempdir().unwrap();
    let (spec, instance) = setup(directory.path());
    let path = instance.dir().join("runtime.json");
    // The record names the VMM's ID with another start time, as after the VMM exited and another
    // process reused the ID, or names another boot, every process of which has exited.
    let tampers: [fn(&mut serde_json::Value); 2] = [
        |record| {
            let start_time = record["process"]["startTime"].as_u64().unwrap();
            record["process"]["startTime"] = (start_time + 1).into();
        },
        |record| {
            let boot = record["process"]["boot"].as_str().unwrap();
            let flipped = if boot.starts_with('0') { "1" } else { "0" };
            record["process"]["boot"] = format!("{flipped}{}", &boot[1..]).into();
        },
    ];
    for tamper in tampers {
        start(&instance, &spec).unwrap();
        let mut record: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let reference: ProcessRef = serde_json::from_value(record["process"].clone()).unwrap();
        let process = Process::open(&reference).unwrap().expect("the VM runs");
        tamper(&mut record);
        fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        // Under another runtime identity too, the VM is found stopped, and its record is cleared,
        // but the process that its ID names is not signaled.
        let mut locked = instance.lock(None).unwrap();
        let found = locked.find(|| Ok(runtime_with('9'))).unwrap();
        assert!(matches!(found, Found::Stopped), "{found:?}");
        drop(locked);
        assert!(records_nothing(&instance));
        assert!(
            !process
                .wait(Instant::now() + Duration::from_millis(200))
                .unwrap()
        );
        process.kill().unwrap();
        assert!(process.wait(Instant::now() + TIMEOUT).unwrap());
    }
}

/// Runs the fake in `directory` as `nvx-rs run` runs OpenVMM on the terminal: with two
/// processors and the backend of this host, and then `arguments`.
fn run_microvm(directory: &Path, arguments: &[&str]) -> std::process::Output {
    let hypervisor = Hypervisor::platform_default().expect("tests run on Linux or Windows");
    let mut command = Command::new(env!("CARGO_BIN_EXE_nvx-fake-openvmm"));
    command
        .args([
            "--single-process",
            "--machine",
            "microvm",
            "--processors",
            "2",
        ])
        .args(["--hypervisor", hypervisor.as_str()])
        .args(arguments)
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    spawn_command(&mut command)
        .unwrap()
        .wait_with_output()
        .unwrap()
}

fn text(data: &[u8]) -> String {
    String::from_utf8_lossy(data).into_owned()
}

#[test]
fn runs_boot_or_restore_a_microvm_on_the_terminal() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let path = |name: &str| root.join(name).to_str().unwrap().to_owned();
    fs::write(root.join("vmlinux"), b"kernel").unwrap();
    fs::write(root.join("initramfs.cpio.gz"), b"initrd").unwrap();
    fs::create_dir(root.join("reports")).unwrap();
    fs::create_dir(root.join("snapshot")).unwrap();
    let (kernel, initrd) = (path("vmlinux"), path("initramfs.cpio.gz"));
    let boot = ["--memory", "128M", "--kernel", &kernel, "--initrd", &initrd];

    // A cold boot prints that it booted, writes its outcome report, records its command line next
    // to the kernel, and exits with 0.
    let report = root.join("reports").join("boot.json");
    let report_path = report.to_str().unwrap().to_owned();
    let mut arguments = boot.to_vec();
    arguments.extend(["--cpu-profile", "host", "--microvm-report", &report_path]);
    let output = run_microvm(root, &arguments);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        "nvx-fake-openvmm: booted the microVM\n"
    );
    assert_eq!(text(&output.stderr), "");
    let document: Value = serde_json::from_str(&fs::read_to_string(&report).unwrap()).unwrap();
    assert_eq!(
        document["outcome"],
        json!({"operation": "run", "category": "success", "status_code": 0})
    );
    assert_eq!(document["network_policy"]["status"], "not-requested");
    let recorded: Vec<String> =
        serde_json::from_str(&fs::read_to_string(root.join("fake-openvmm-reports.json")).unwrap())
            .unwrap();
    assert_eq!(recorded[recorded.len() - 1], report_path);

    // The kernel command line sets the status, also of a restore, which prints that it restored
    // the snapshot and records its command line next to it.
    let mut arguments = boot.to_vec();
    arguments.extend(["--cmdline", "quiet fake_exit_status=5"]);
    let output = run_microvm(root, &arguments);
    assert_eq!(output.status.code(), Some(5), "{}", text(&output.stderr));
    let snapshot = path("snapshot");
    let restore = [
        "--restore-snapshot",
        &snapshot,
        "--restore-processors",
        "1",
        "--cmdline",
        "fake_exit_status=6",
    ];
    fs::remove_file(root.join("fake-openvmm-.json")).unwrap();
    let output = run_microvm(root, &restore);
    assert_eq!(output.status.code(), Some(6), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        "nvx-fake-openvmm: restored the microVM\n"
    );
    let recorded: Vec<String> =
        serde_json::from_str(&fs::read_to_string(root.join("fake-openvmm-.json")).unwrap())
            .unwrap();
    assert_eq!(recorded[recorded.len() - restore.len()..], restore);

    // A restore without its snapshot fails as OpenVMM fails it, and OpenVMM refuses a command
    // line that it does not take with status 2.
    let missing = path("missing");
    let output = run_microvm(root, &["--restore-snapshot", &missing]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(text(&output.stdout), "");
    assert_eq!(
        text(&output.stderr),
        format!("nvx-fake-openvmm: cannot restore {missing}: no such snapshot directory\n")
    );
    let output = run_microvm(root, &["--restore-snapshot", &snapshot, "--memory", "128M"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        text(&output.stderr),
        "nvx-fake-openvmm: a restore takes no --memory\n"
    );

    // As OpenVMM does, a restore takes the network profile and policy of a snapshot that has a NIC
    // without `--net`, whose addressing the snapshot saved, and refuses `--net`.
    let networked = [
        "--restore-snapshot",
        &snapshot,
        "--network-profile",
        "portable",
        "--network-egress",
        "deny",
    ];
    let output = run_microvm(root, &networked);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        "nvx-fake-openvmm: restored the microVM\n"
    );
    let output = run_microvm(root, &[&networked[..], &["--net", "10.0.0.2/24"]].concat());
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        text(&output.stderr),
        "nvx-fake-openvmm: restore takes microVM network addressing from saved state\n"
    );
}

#[test]
fn runs_find_absolute_policy_paths_in_relative_share_roots() {
    // nvx-rs run passes the host path of each share as given, which may be relative to the
    // working directory, and with several shares, absolute policy paths, which OpenVMM
    // attributes to the share whose root contains them once it has resolved both.
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::write(root.join("vmlinux"), b"kernel").unwrap();
    fs::write(root.join("initramfs.cpio.gz"), b"initrd").unwrap();
    for path in ["share/hidden/shown", "share/writable", "other", "outside"] {
        fs::create_dir_all(root.join(path)).unwrap();
    }
    let absolute = |relative: &str| root.join(relative).to_str().unwrap().to_owned();
    let (kernel, initrd) = (absolute("vmlinux"), absolute("initramfs.cpio.gz"));
    let boot = ["--memory", "128M", "--kernel", &kernel, "--initrd", &initrd];
    let share = format!("{},share,rw", aggregate_child_name(0, "/data"));
    let other = format!("{},other,ro", aggregate_child_name(1, "/other"));
    let aggregate = [
        "--mount-aggregate",
        "/run/nvx/shares",
        "--mount-child",
        &share,
        "--mount-child",
        &other,
    ];
    let (hidden, shown, writable) = (
        absolute("share/hidden"),
        absolute("share/hidden/shown"),
        absolute("share/writable"),
    );
    let policy = [
        "--mount-deny",
        &hidden,
        "--mount-allow",
        &shown,
        "--mount-write",
        &writable,
    ];
    let mut arguments = boot.to_vec();
    arguments.extend(aggregate);
    arguments.extend(policy);
    let output = run_microvm(root, &arguments);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        "nvx-fake-openvmm: booted the microVM\n"
    );

    // One share with a relative root takes absolute policy paths too, beside the relative ones
    // that OpenVMM resolves in it.
    let mut arguments = boot.to_vec();
    arguments.extend([
        "--mount",
        "/data,share,rw",
        "--mount-deny",
        &hidden,
        "--mount-allow",
        "hidden/shown",
    ]);
    let output = run_microvm(root, &arguments);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));

    // A path outside every share stays refused, as one that does not exist does.
    for path in [absolute("outside"), absolute("share/missing")] {
        let mut arguments = boot.to_vec();
        arguments.extend(aggregate);
        arguments.extend(["--mount-deny", &path]);
        let output = run_microvm(root, &arguments);
        assert_eq!(output.status.code(), Some(2), "{path}");
        assert_eq!(
            text(&output.stderr),
            format!("nvx-fake-openvmm: invalid --mount-deny {path}\n")
        );
    }
}
