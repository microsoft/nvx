//! Drives the OpenVMM backend end to end against the `aci-edge-sandboxes-fake-openvmm` test double, which
//! serves the real control protocol without running a VM.
#![cfg(all(
    feature = "openvmm",
    feature = "testing",
    any(target_os = "linux", windows)
))]

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use aci_edge_sandboxes::openvmm::{Hypervisor, OpenVmmConfig};
use aci_edge_sandboxes::{
    Access, AciEdgeSandbox, EgressPolicy, ErrorCode, ExecFailure, ExecOutcome, ExecOutput,
    ExecRequest, FilesystemPolicy, NetworkPolicy, NetworkRule, Protocol, ProvisionRequest,
    SandboxId, StdinMode,
};
use tempfile::TempDir;

mod support;

struct Fixture {
    directory: TempDir,
    state_root: PathBuf,
}

struct PendingVm {
    child: Child,
}

impl PendingVm {
    fn spawn(arguments: &[String], capability: &[u8], directory: &Path) -> Self {
        let (reader, mut writer) = std::io::pipe().unwrap();
        writer.write_all(capability).unwrap();
        drop(writer);
        Self {
            child: Command::new(env!("CARGO_BIN_EXE_aci-edge-sandboxes-fake-openvmm"))
                .args(arguments)
                .current_dir(directory)
                .stdin(Stdio::from(reader))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        }
    }

    #[cfg(target_os = "linux")]
    fn start_time(&self) -> u64 {
        let stat = fs::read_to_string(format!("/proc/{}/stat", self.child.id())).unwrap();
        stat.rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .nth(19)
            .unwrap()
            .parse()
            .unwrap()
    }

    #[cfg(windows)]
    fn start_time(&self) -> u64 {
        use std::os::windows::io::AsRawHandle;

        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::Threading::GetProcessTimes;

        let mut created = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut exited = created;
        let mut kernel = created;
        let mut user = created;
        // SAFETY: Child owns the live process handle and all outputs point to valid storage.
        let succeeded = unsafe {
            GetProcessTimes(
                self.child.as_raw_handle().cast(),
                &mut created,
                &mut exited,
                &mut kernel,
                &mut user,
            )
        };
        assert_ne!(succeeded, 0, "{}", std::io::Error::last_os_error());
        (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime)
    }
}

impl Drop for PendingVm {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
        }
        self.child.wait().unwrap();
    }
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("vmlinux"), b"kernel").unwrap();
        fs::write(directory.path().join("initramfs.cpio.gz"), b"initrd").unwrap();
        let state_root = directory.path().join("state");
        Self {
            directory,
            state_root,
        }
    }

    fn config(&self) -> OpenVmmConfig {
        let mut config = OpenVmmConfig::new(
            env!("CARGO_BIN_EXE_aci-edge-sandboxes-fake-openvmm"),
            self.directory.path().join("vmlinux"),
            self.directory.path().join("initramfs.cpio.gz"),
            Hypervisor::platform_default().expect("tests run on Linux or Windows"),
            &self.state_root,
        );
        config.skip_hypervisor_probe = true;
        config.start_timeout = Duration::from_secs(30);
        config.control_timeout = Duration::from_secs(15);
        config.stop_timeout = Duration::from_secs(15);
        config
    }

    fn nvx(&self) -> AciEdgeSandbox {
        self.nvx_with(|_| {})
    }

    fn nvx_with(&self, configure: impl FnOnce(&mut OpenVmmConfig)) -> AciEdgeSandbox {
        let mut config = self.config();
        configure(&mut config);
        AciEdgeSandbox::openvmm(config).unwrap()
    }

    fn launch_arguments(&self, sandbox_id: &SandboxId) -> Vec<String> {
        let path = self
            .directory
            .path()
            .join(format!("fake-openvmm-{}.json", sandbox_id.token()));
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    fn sandbox_dirs(&self) -> usize {
        fs::read_dir(&self.state_root)
            .unwrap()
            .filter(|entry| {
                !entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with('.')
            })
            .count()
    }
}

fn run(nvx: &AciEdgeSandbox, sandbox_id: &SandboxId, script: &str) -> ExecOutput {
    nvx.exec(sandbox_id, &ExecRequest::command_line(script))
        .unwrap()
        .wait_with_output()
        .unwrap()
}

fn has_pair(arguments: &[String], name: &str, value: &str) -> bool {
    arguments
        .windows(2)
        .any(|pair| pair[0] == name && pair[1] == value)
}

fn state_json(fixture: &Fixture, sandbox_id: &SandboxId, name: &str) -> serde_json::Value {
    let path = fixture.state_root.join(sandbox_id.token()).join(name);
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn full_lifecycle_streams_output_and_keeps_state_until_stop() {
    let fixture = Fixture::new();
    let nvx = fixture.nvx();
    let sandbox_id = nvx.provision(&ProvisionRequest::new()).unwrap().sandbox_id;
    let started = nvx.start(&sandbox_id).unwrap();
    assert!(started.metadata.unwrap().contains_key("bootMilliseconds"));

    let output = run(&nvx, &sandbox_id, "echo hello; echoerr warning; exit 3");
    assert_eq!(output.stdout, b"hello\n");
    assert_eq!(output.stderr, b"warning\n");
    assert_eq!(output.outcome, ExecOutcome::Exited(3));
    assert!(
        run(&nvx, &sandbox_id, "write greeting persisted")
            .outcome
            .success()
    );
    assert_eq!(run(&nvx, &sandbox_id, "read greeting").stdout, b"persisted");

    let stopped = nvx.stop(&sandbox_id).unwrap().metadata.unwrap();
    assert_eq!(stopped["forced"], false);
    assert_eq!(stopped["vmOutcome"], "success");
    assert_eq!(stopped["teardownComplete"], true);

    // The guest's root file system lives in memory, so a restart begins with fresh state.
    nvx.start(&sandbox_id).unwrap();
    assert!(run(&nvx, &sandbox_id, "read greeting").stdout.is_empty());
    nvx.stop(&sandbox_id).unwrap();
    nvx.deprovision(&sandbox_id).unwrap();
    assert!(!fixture.state_root.join(sandbox_id.token()).exists());
}

#[test]
fn state_machine_violations_use_contract_codes() {
    let fixture = Fixture::new();
    let nvx = fixture.nvx();
    let sandbox_id = nvx.provision(&ProvisionRequest::new()).unwrap().sandbox_id;
    let echo = ExecRequest::command_line("echo");
    assert_eq!(
        nvx.exec(&sandbox_id, &echo).unwrap_err().code(),
        ErrorCode::NotStarted
    );
    assert_eq!(
        nvx.stop(&sandbox_id).unwrap_err().code(),
        ErrorCode::AlreadyStopped
    );
    nvx.start(&sandbox_id).unwrap();
    assert_eq!(
        nvx.start(&sandbox_id).unwrap_err().code(),
        ErrorCode::AlreadyStarted
    );
    assert_eq!(
        nvx.deprovision(&sandbox_id).unwrap_err().code(),
        ErrorCode::AlreadyStarted
    );
    nvx.stop(&sandbox_id).unwrap();
    nvx.deprovision(&sandbox_id).unwrap();
    assert_eq!(
        nvx.start(&sandbox_id).unwrap_err().code(),
        ErrorCode::StaleId
    );
    assert_eq!(
        nvx.stop(&sandbox_id).unwrap_err().code(),
        ErrorCode::StaleId
    );
    assert_eq!(
        nvx.exec(&sandbox_id, &echo).unwrap_err().code(),
        ErrorCode::StaleId
    );
    assert_eq!(
        nvx.deprovision(&sandbox_id).unwrap_err().code(),
        ErrorCode::StaleId
    );
}

#[test]
fn workload_outcomes_are_distinguished() {
    let fixture = Fixture::new();
    let nvx = fixture.nvx();
    let sandbox_id = nvx.provision(&ProvisionRequest::new()).unwrap().sandbox_id;
    nvx.start(&sandbox_id).unwrap();

    let timed_out = nvx
        .exec(
            &sandbox_id,
            &ExecRequest::command_line("echo started; sleep 5000")
                .with_timeout(Duration::from_millis(200)),
        )
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert_eq!(timed_out.outcome, ExecOutcome::TimedOut);
    assert_eq!(timed_out.stdout, b"started\n");
    assert_eq!(
        run(&nvx, &sandbox_id, "signal 9").outcome,
        ExecOutcome::Signaled(9)
    );
    let flood = run(&nvx, &sandbox_id, "flood 2000000");
    assert_eq!(
        flood.outcome,
        ExecOutcome::Failed(ExecFailure::OutputLimitExceeded)
    );
    assert!(flood.stdout.len() <= 1024 * 1024);
    assert_eq!(
        run(&nvx, &sandbox_id, "launchfail").outcome,
        ExecOutcome::Failed(ExecFailure::LaunchFailed)
    );
    assert_eq!(
        run(&nvx, &sandbox_id, "fail").outcome,
        ExecOutcome::Failed(ExecFailure::Workload)
    );
    let argv = nvx
        .exec(
            &sandbox_id,
            &ExecRequest::argv(["/bin/echo", "argv", "form"]),
        )
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert_eq!(argv.stdout, b"argv form\n");
    let missing = nvx
        .exec(&sandbox_id, &ExecRequest::argv(["/usr/bin/missing"]))
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(missing, ExecOutcome::Exited(127));

    let started = Instant::now();
    let execution = nvx
        .exec(
            &sandbox_id,
            &ExecRequest::command_line("echo started; sleep 30000; echo late"),
        )
        .unwrap();
    let canceller = execution.canceller();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        canceller.cancel().unwrap();
    });
    let cancelled = execution.wait_with_output().unwrap();
    assert_eq!(cancelled.outcome, ExecOutcome::Cancelled);
    assert_eq!(cancelled.stdout, b"started\n");
    assert!(started.elapsed() < Duration::from_secs(10));

    // Cancelling a finished execution changes nothing, and the sandbox stays usable.
    let finished = nvx
        .exec(&sandbox_id, &ExecRequest::command_line("echo done"))
        .unwrap();
    let canceller = finished.canceller();
    assert_eq!(finished.wait_with_output().unwrap().stdout, b"done\n");
    canceller.cancel().unwrap();
    assert_eq!(run(&nvx, &sandbox_id, "echo after").stdout, b"after\n");

    nvx.stop(&sandbox_id).unwrap();
    nvx.deprovision(&sandbox_id).unwrap();
}

#[test]
fn unsupported_requests_are_rejected_before_anything_runs() {
    let fixture = Fixture::new();
    let nvx = fixture.nvx();

    let missing = ProvisionRequest::new().with_filesystem(FilesystemPolicy {
        readonly_paths: vec![fixture.directory.path().join("missing")],
        ..FilesystemPolicy::default()
    });
    assert_eq!(
        nvx.provision(&missing).unwrap_err().code(),
        ErrorCode::PolicyValidation
    );
    let ipv6 = NetworkPolicy {
        egress: EgressPolicy::new(Access::Deny).with_allow(NetworkRule::to("2001:db8::/32")),
        ..NetworkPolicy::deny_all()
    };
    assert_eq!(
        nvx.provision(&ProvisionRequest::new().with_network(ipv6))
            .unwrap_err()
            .code(),
        ErrorCode::PolicyValidation
    );
    let mut ingress = NetworkPolicy::deny_all();
    ingress.ingress.default = Access::Allow;
    assert_eq!(
        nvx.provision(&ProvisionRequest::new().with_network(ingress))
            .unwrap_err()
            .code(),
        ErrorCode::PolicyValidation
    );
    let relative = ProvisionRequest::new().with_filesystem(FilesystemPolicy {
        readonly_paths: vec![PathBuf::from("relative")],
        ..FilesystemPolicy::default()
    });
    assert_eq!(
        nvx.provision(&relative).unwrap_err().code(),
        ErrorCode::MalformedRequest
    );
    assert_eq!(
        nvx.provision(&ProvisionRequest::new().with_memory_mib(0))
            .unwrap_err()
            .code(),
        ErrorCode::MalformedRequest
    );
    assert_eq!(fixture.sandbox_dirs(), 0);

    let sandbox_id = nvx.provision(&ProvisionRequest::new()).unwrap().sandbox_id;
    assert_eq!(fixture.sandbox_dirs(), 1);

    nvx.start(&sandbox_id).unwrap();
    let write = || ExecRequest::command_line("write rejected ran");
    for request in [
        write().with_cwd("relative"),
        write().with_stdin(StdinMode::Piped),
        write().with_timeout(Duration::from_secs(2 * 60 * 60)),
        write().with_env(format!("BIG={}", "x".repeat(5000))),
        write().with_environment((0..100).map(|index| format!("V{index}=x"))),
        ExecRequest::argv(["relative/program"]),
        ExecRequest::argv(["relative/program"]).with_env("FOO=bar"),
        ExecRequest::command_line("x".repeat(5000)),
    ] {
        assert_eq!(
            nvx.exec(&sandbox_id, &request).unwrap_err().code(),
            ErrorCode::PolicyValidation,
            "{request:?}"
        );
    }
    for request in [
        write().with_env("NOVALUE"),
        write().with_env("=value"),
        write().with_environment(["A=1", "B"]),
    ] {
        assert_eq!(
            nvx.exec(&sandbox_id, &request).unwrap_err().code(),
            ErrorCode::MalformedRequest,
            "{request:?}"
        );
    }
    assert!(run(&nvx, &sandbox_id, "read rejected").stdout.is_empty());
    nvx.stop(&sandbox_id).unwrap();
    nvx.deprovision(&sandbox_id).unwrap();
}

#[test]
fn sandboxes_are_shared_across_backend_instances() {
    let fixture = Fixture::new();
    let first = fixture.nvx();
    let sandbox_id = first
        .provision(&ProvisionRequest::new())
        .unwrap()
        .sandbox_id;
    first.start(&sandbox_id).unwrap();
    drop(first);

    let second = fixture.nvx();
    assert_eq!(
        run(&second, &sandbox_id, "echo from another instance").stdout,
        b"from another instance\n"
    );
    second.stop(&sandbox_id).unwrap();
    second.deprovision(&sandbox_id).unwrap();
}

#[test]
fn crashed_vms_fall_back_to_provisioned() {
    let fixture = Fixture::new();
    let nvx = fixture.nvx_with(|config| {
        config.kernel_command_line = "fake_crash_after_ms=1000".to_owned();
    });
    let sandbox_id = nvx.provision(&ProvisionRequest::new()).unwrap().sandbox_id;
    nvx.start(&sandbox_id).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match nvx.exec(&sandbox_id, &ExecRequest::command_line("echo alive")) {
            Err(error) => {
                assert_eq!(error.code(), ErrorCode::NotStarted, "{error}");
                break;
            }
            Ok(execution) => {
                let _ = execution.wait();
            }
        }
        assert!(Instant::now() < deadline, "the crash was never observed");
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        nvx.stop(&sandbox_id).unwrap_err().code(),
        ErrorCode::AlreadyStopped
    );
    nvx.deprovision(&sandbox_id).unwrap();
}

#[test]
fn unresponsive_guests_are_stopped_forcibly() {
    let fixture = Fixture::new();
    let nvx = fixture.nvx_with(|config| {
        config.kernel_command_line = "fake_ignore_stop=1".to_owned();
        config.stop_timeout = Duration::from_millis(500);
    });
    let sandbox_id = nvx.provision(&ProvisionRequest::new()).unwrap().sandbox_id;
    nvx.start(&sandbox_id).unwrap();
    let stopped = nvx.stop(&sandbox_id).unwrap().metadata.unwrap();
    assert_eq!(stopped["forced"], true);
    assert_eq!(
        nvx.exec(&sandbox_id, &ExecRequest::command_line("echo"))
            .unwrap_err()
            .code(),
        ErrorCode::NotStarted
    );
    nvx.deprovision(&sandbox_id).unwrap();
}

#[test]
fn failed_starts_leave_the_sandbox_provisioned() {
    let fixture = Fixture::new();
    let failing = fixture.nvx_with(|config| {
        config.kernel_command_line = "fake_exit_on_start=4".to_owned();
    });
    let sandbox_id = failing
        .provision(&ProvisionRequest::new())
        .unwrap()
        .sandbox_id;
    let error = failing.start(&sandbox_id).unwrap_err();
    assert_eq!(error.code(), ErrorCode::BackendError);
    assert!(error.message().contains("openvmm.log"), "{error}");

    let healthy = fixture.nvx();
    healthy.start(&sandbox_id).unwrap();
    assert!(
        run(&healthy, &sandbox_id, "echo recovered")
            .outcome
            .success()
    );
    healthy.stop(&sandbox_id).unwrap();
    healthy.deprovision(&sandbox_id).unwrap();
}

#[test]
fn guests_without_the_required_features_are_refused() {
    let fixture = Fixture::new();
    // This guest agent predates the features request, like the release that older bundles pin.
    let legacy = fixture.nvx_with(|config| {
        config.kernel_command_line = "fake_legacy_guest=1".to_owned();
    });
    let sandbox_id = legacy
        .provision(&ProvisionRequest::new())
        .unwrap()
        .sandbox_id;
    let error = legacy.start(&sandbox_id).unwrap_err();
    assert_eq!(error.code(), ErrorCode::BackendUnavailable, "{error}");
    for feature in [
        "cancellation",
        "host path mappings",
        "workload accounts",
        "workload containment",
    ] {
        assert!(error.message().contains(feature), "{error}");
    }

    // The refused VM was terminated and forgotten, so the sandbox is merely provisioned.
    assert!(
        !fixture
            .state_root
            .join(sandbox_id.token())
            .join("runtime.json")
            .exists()
    );
    assert_eq!(
        legacy
            .exec(&sandbox_id, &ExecRequest::command_line("echo"))
            .unwrap_err()
            .code(),
        ErrorCode::NotStarted
    );

    let current = fixture.nvx();
    current.start(&sandbox_id).unwrap();
    assert!(
        run(&current, &sandbox_id, "echo recovered")
            .outcome
            .success()
    );
    current.stop(&sandbox_id).unwrap();
    current.deprovision(&sandbox_id).unwrap();
}

#[test]
fn interrupted_starts_do_not_leak_vms() {
    let fixture = Fixture::new();
    let nvx = fixture.nvx();
    let sandbox_id = nvx.provision(&ProvisionRequest::new()).unwrap().sandbox_id;
    nvx.start(&sandbox_id).unwrap();

    let runtime = state_json(&fixture, &sandbox_id, "runtime.json");
    let orphan = support::interrupt_start(&fixture.state_root, &sandbox_id);
    assert!(support::process_running(orphan));

    nvx.start(&sandbox_id).unwrap();
    assert!(
        !support::process_running(orphan),
        "the orphaned VM still runs"
    );
    let dir = fixture.state_root.join(sandbox_id.token());
    assert!(!dir.join("launch.json").exists());
    let restarted = state_json(&fixture, &sandbox_id, "runtime.json");
    assert_ne!(restarted["pid"], runtime["pid"]);
    assert_eq!(
        run(&nvx, &sandbox_id, "echo recovered").stdout,
        b"recovered\n"
    );
    nvx.stop(&sandbox_id).unwrap();
    nvx.deprovision(&sandbox_id).unwrap();
}

#[test]
fn interrupted_starts_before_an_endpoint_recover_or_retain_the_child() {
    for identified in [true, false] {
        let fixture = Fixture::new();
        let nvx = fixture.nvx();
        let sandbox_id = nvx.provision(&ProvisionRequest::new()).unwrap().sandbox_id;
        nvx.start(&sandbox_id).unwrap();
        let runtime = state_json(&fixture, &sandbox_id, "runtime.json");
        let dir = fixture.state_root.join(sandbox_id.token());
        let capability = fs::read(dir.join("control.capability")).unwrap();
        let mut arguments = fixture.launch_arguments(&sandbox_id);
        let command_line = arguments
            .iter()
            .position(|argument| argument == "--cmdline")
            .unwrap()
            + 1;
        arguments[command_line].push_str(" fake_boot_delay_ms=30000");
        nvx.stop(&sandbox_id).unwrap();
        fs::remove_file(dir.join("outcome.json")).unwrap();

        let child = PendingVm::spawn(&arguments, &capability, &dir);
        let pid = child.child.id();
        let mut launch = serde_json::json!({
            "format": runtime["format"],
            "endpoint": runtime["endpoint"],
        });
        if identified {
            launch["process"] = serde_json::json!({ "pid": pid, "startTime": child.start_time() });
        }
        let marker = dir.join("launch.json");
        fs::write(&marker, launch.to_string()).unwrap();
        fs::File::options()
            .write(true)
            .open(&marker)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(120))
            .unwrap();
        assert!(support::process_running(pid));
        assert!(!dir.join("runtime.json").exists());

        let result = nvx.start(&sandbox_id);
        if identified {
            result.unwrap();
            assert!(
                !support::process_running(pid),
                "the pre-endpoint child survived recovery"
            );
            assert!(!marker.exists());
            assert_ne!(
                state_json(&fixture, &sandbox_id, "runtime.json")["pid"],
                pid
            );
            assert_eq!(
                run(&nvx, &sandbox_id, "echo recovered").stdout,
                b"recovered\n"
            );
            nvx.stop(&sandbox_id).unwrap();
            nvx.deprovision(&sandbox_id).unwrap();
        } else {
            if result.is_ok() {
                nvx.stop(&sandbox_id).unwrap();
            }
            assert_eq!(result.unwrap_err().code(), ErrorCode::BackendError);
            assert!(support::process_running(pid));
            assert!(marker.exists());
            assert!(!dir.join("runtime.json").exists());
            assert_eq!(
                nvx.deprovision(&sandbox_id).unwrap_err().code(),
                ErrorCode::BackendError
            );
        }
    }
}

#[test]
fn launch_markers_without_process_identity_do_not_expire() {
    let fixture = Fixture::new();
    let nvx = fixture.nvx();
    let sandbox_id = nvx.provision(&ProvisionRequest::new()).unwrap().sandbox_id;
    let dir = fixture.state_root.join(sandbox_id.token());
    let endpoint = if cfg!(windows) {
        format!("//./pipe/openvmm-microvm-{}", sandbox_id.token())
    } else {
        dir.join("control.sock").to_string_lossy().into_owned()
    };
    let marker = dir.join("launch.json");
    let format = state_json(&fixture, &sandbox_id, "sandbox.json")["format"].clone();
    let launch = serde_json::json!({ "format": format, "endpoint": endpoint });
    fs::write(&marker, launch.to_string()).unwrap();

    // An absent endpoint does not prove that the interrupted launch has no surviving child.
    for error in [
        nvx.start(&sandbox_id).unwrap_err(),
        nvx.deprovision(&sandbox_id).unwrap_err(),
    ] {
        assert_eq!(error.code(), ErrorCode::BackendError);
    }
    assert!(marker.exists());

    fs::File::options()
        .write(true)
        .open(&marker)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(120))
        .unwrap();
    let result = nvx.start(&sandbox_id);
    if result.is_ok() {
        nvx.stop(&sandbox_id).unwrap();
    }
    assert_eq!(result.unwrap_err().code(), ErrorCode::BackendError);
    assert!(marker.exists());
    assert_eq!(
        nvx.deprovision(&sandbox_id).unwrap_err().code(),
        ErrorCode::BackendError
    );
}

#[test]
fn cancellation_has_a_response_deadline_for_untimed_and_long_timed_workloads() {
    for timeout in [None, Some(Duration::from_secs(30))] {
        let fixture = Fixture::new();
        let nvx = fixture.nvx_with(|config| {
            config.kernel_command_line = "fake_ignore_cancel=1".to_owned();
            config.control_timeout = Duration::from_millis(200);
            config.stop_timeout = Duration::from_millis(200);
        });
        let sandbox_id = nvx.provision(&ProvisionRequest::new()).unwrap().sandbox_id;
        nvx.start(&sandbox_id).unwrap();
        let mut request = ExecRequest::command_line("echo started; sleep 10000");
        request.process.timeout = timeout;
        let mut execution = nvx.exec(&sandbox_id, &request).unwrap();
        let mut stdout = execution.take_stdout().unwrap();
        let mut started = [0u8; 8];
        stdout.read_exact(&mut started).unwrap();
        assert_eq!(&started, b"started\n");
        drop(stdout);
        execution.canceller().cancel().unwrap();
        let (finished, received) = mpsc::channel();
        let worker = thread::spawn(move || finished.send(execution.wait()).unwrap());
        let started = Instant::now();
        let outcome = received.recv_timeout(Duration::from_secs(2));
        let elapsed = started.elapsed();
        nvx.stop(&sandbox_id).unwrap();
        worker.join().unwrap();
        nvx.deprovision(&sandbox_id).unwrap();
        let error = outcome
            .expect("cancellation must not wait for an unresponsive guest indefinitely")
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::BackendError);
        assert!(error.message().contains("timed out"), "{error}");
        assert!(elapsed < Duration::from_secs(2));
    }
}

#[test]
fn slow_boots_are_awaited() {
    let fixture = Fixture::new();
    let nvx = fixture.nvx_with(|config| {
        config.kernel_command_line = "fake_boot_delay_ms=500".to_owned();
    });
    let sandbox_id = nvx.provision(&ProvisionRequest::new()).unwrap().sandbox_id;
    let metadata = nvx.start(&sandbox_id).unwrap().metadata.unwrap();
    assert!(metadata["bootMilliseconds"].as_u64().unwrap() >= 500);
    nvx.stop(&sandbox_id).unwrap();
    nvx.deprovision(&sandbox_id).unwrap();
}

#[test]
fn launch_arguments_select_the_managed_alpine_guest() {
    let fixture = Fixture::new();
    let nvx = fixture.nvx();
    let request = ProvisionRequest::new()
        .with_network(NetworkPolicy::egress(Access::Allow))
        .with_memory_mib(512);
    let sandbox_id = nvx.provision(&request).unwrap().sandbox_id;
    nvx.start(&sandbox_id).unwrap();
    let arguments = fixture.launch_arguments(&sandbox_id);
    assert!(has_pair(&arguments, "--microvm-lifecycle", "managed"));
    assert!(has_pair(&arguments, "--memory", "512M"));
    assert!(has_pair(
        &arguments,
        "--microvm-workload-identity",
        "65534:65534"
    ));
    assert!(has_pair(&arguments, "--network-egress", "allow"));
    assert!(has_pair(&arguments, "--network-ingress", "deny"));
    assert!(has_pair(&arguments, "--host-loopback", "deny"));
    assert!(
        arguments
            .iter()
            .any(|argument| argument == "--microvm-control-auth-stdin")
    );
    assert!(has_pair(&arguments, "--cmdline", "hostname=nvx-sandbox"));
    assert!(
        !arguments
            .iter()
            .any(|argument| argument == "--microvm-sandbox-block")
    );
    nvx.stop(&sandbox_id).unwrap();
    nvx.deprovision(&sandbox_id).unwrap();

    let isolated = ProvisionRequest::new().with_network(NetworkPolicy::deny_all());
    let sandbox_id = nvx.provision(&isolated).unwrap().sandbox_id;
    nvx.start(&sandbox_id).unwrap();
    assert!(
        !fixture
            .launch_arguments(&sandbox_id)
            .iter()
            .any(|argument| argument == "--net")
    );
    nvx.stop(&sandbox_id).unwrap();
    nvx.deprovision(&sandbox_id).unwrap();
}

#[test]
fn filesystem_and_network_policies_reach_openvmm() {
    let fixture = Fixture::new();
    let nvx = fixture.nvx();
    let base = fixture.directory.path();
    for name in ["work/src/secret", "work/out"] {
        fs::create_dir_all(base.join(name)).unwrap();
    }
    let request = ProvisionRequest::new()
        .with_filesystem(FilesystemPolicy {
            readonly_paths: vec![base.join("work").join("src")],
            readwrite_paths: vec![base.join("work").join("out")],
            denied_paths: vec![base.join("work").join("src").join("secret")],
        })
        .with_network(NetworkPolicy {
            egress: EgressPolicy::new(Access::Deny)
                .with_allow(NetworkRule::to("192.0.2.0/24").on_port(Protocol::Tcp, 443)),
            ..NetworkPolicy::deny_all()
        });
    let sandbox_id = nvx.provision(&request).unwrap().sandbox_id;
    nvx.start(&sandbox_id).unwrap();
    let arguments = fixture.launch_arguments(&sandbox_id);
    let value = |name: &str| {
        arguments
            .windows(2)
            .filter(|pair| pair[0] == name)
            .map(|pair| pair[1].clone())
            .collect::<Vec<_>>()
    };
    let mounts = value("--mount");
    assert_eq!(mounts.len(), 1);
    assert!(mounts[0].starts_with("/run/nvx/hostfs/root,"), "{mounts:?}");
    assert!(mounts[0].ends_with(",rw"), "{mounts:?}");
    assert_eq!(value("--mount-deny").len(), 1);
    assert_eq!(value("--network-egress"), ["deny"]);
    assert_eq!(value("--network-egress-allow"), ["192.0.2.0/24:tcp:443"]);
    let command_line = &value("--cmdline")[0];
    let maps: Vec<&str> = command_line
        .split(' ')
        .filter(|token| token.starts_with("nvx_map="))
        .collect();
    assert_eq!(maps.len(), 2, "{command_line}");
    assert!(maps.iter().any(|token| token.ends_with(",ro")));
    assert!(maps.iter().any(|token| token.ends_with(",rw")));

    let guest =
        aci_edge_sandboxes::openvmm::resolve_guest_path(&base.join("work").join("out")).unwrap();
    assert!(
        state_json(&fixture, &sandbox_id, "sandbox.json")["filesystem"]["binds"]
            .as_array()
            .unwrap()
            .iter()
            .any(|bind| bind["source"] == "out" && bind["target"] == guest),
        "the working directory must match the canonical mapped guest path"
    );
    let pwd = nvx
        .exec(
            &sandbox_id,
            &ExecRequest::command_line("pwd").with_cwd(guest.clone()),
        )
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert_eq!(pwd.stdout, format!("{guest}\n").into_bytes());
    nvx.stop(&sandbox_id).unwrap();
    nvx.deprovision(&sandbox_id).unwrap();
}

/// What the guest's default environment always contains, which the fake guest emulates.
const DEFAULT_ENVIRONMENT: [&str; 5] = [
    "PATH=/usr/sbin:/usr/bin:/sbin:/bin",
    "TERM=linux",
    "HOME=/",
    "USER=nobody",
    "LOGNAME=nobody",
];

fn sorted<I, S>(lines: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut lines: Vec<String> = lines.into_iter().map(Into::into).collect();
    lines.sort();
    lines
}

fn output_of(nvx: &AciEdgeSandbox, sandbox_id: &SandboxId, request: ExecRequest) -> ExecOutput {
    let output = nvx
        .exec(sandbox_id, &request)
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert!(output.outcome.success(), "{request:?}: {output:?}");
    assert!(output.stderr.is_empty(), "{request:?}: {output:?}");
    output
}

/// Returns the sorted `NAME=VALUE` lines that the workload of `request` prints.
fn environment_of(
    nvx: &AciEdgeSandbox,
    sandbox_id: &SandboxId,
    request: ExecRequest,
) -> Vec<String> {
    let output = output_of(nvx, sandbox_id, request);
    sorted(String::from_utf8(output.stdout).unwrap().lines())
}

#[test]
fn environments_follow_the_mxc_schema() {
    let fixture = Fixture::new();
    let nvx = fixture.nvx();
    let capabilities = nvx.capabilities().exec;
    assert!(capabilities.env && capabilities.clear_default_env);
    let sandbox_id = nvx.provision(&ProvisionRequest::new()).unwrap().sandbox_id;
    nvx.start(&sandbox_id).unwrap();
    // Programs run directly, because a shell exports variables of its own.
    let print = || ExecRequest::argv(["/usr/bin/env"]);
    let environment = |request| environment_of(&nvx, &sandbox_id, request);

    // No environment supplied: the default environment, whatever `inheritDefaultEnv` says.
    assert_eq!(environment(print()), sorted(DEFAULT_ENVIRONMENT));
    assert_eq!(
        environment(print().with_inherit_default_env(false)),
        sorted(DEFAULT_ENVIRONMENT)
    );
    assert_eq!(
        environment(print().with_inherit_default_env(true)),
        sorted(DEFAULT_ENVIRONMENT)
    );

    // An explicitly empty environment is empty, not the default one.
    assert_eq!(
        environment(print().with_environment(Vec::<String>::new())),
        Vec::<String>::new()
    );
    assert_eq!(
        environment(
            print()
                .with_environment(Vec::<String>::new())
                .with_inherit_default_env(false)
        ),
        Vec::<String>::new()
    );

    // Supplied entries are the whole environment, empty values included.
    assert_eq!(
        environment(print().with_environment(["FOO=bar", "EMPTY="])),
        sorted(["FOO=bar", "EMPTY="])
    );
    assert_eq!(
        environment(print().with_env("FOO=bar").with_env("EMPTY=")),
        sorted(["FOO=bar", "EMPTY="])
    );
    assert_eq!(
        environment(print().with_env("FOO=bar").with_inherit_default_env(false)),
        sorted(["FOO=bar"])
    );
    // The last of repeated names wins.
    assert_eq!(
        environment(print().with_environment(["A=1", "B=2", "A=3"])),
        sorted(["A=3", "B=2"])
    );

    // Layering keeps the default environment, and a supplied entry wins over a default.
    assert_eq!(
        environment(
            print()
                .with_environment(["FOO=bar", "PATH=/custom"])
                .with_inherit_default_env(true)
        ),
        sorted([
            "PATH=/custom",
            "TERM=linux",
            "HOME=/",
            "USER=nobody",
            "LOGNAME=nobody",
            "FOO=bar",
        ])
    );
    assert_eq!(
        environment(
            print()
                .with_environment(Vec::<String>::new())
                .with_inherit_default_env(true)
        ),
        sorted(DEFAULT_ENVIRONMENT)
    );
    nvx.stop(&sandbox_id).unwrap();
    nvx.deprovision(&sandbox_id).unwrap();
}

#[test]
fn environment_values_reach_the_workload_exactly() {
    let fixture = Fixture::new();
    let nvx = fixture.nvx();
    let sandbox_id = nvx.provision(&ProvisionRequest::new()).unwrap().sandbox_id;
    nvx.start(&sandbox_id).unwrap();
    let entries = [
        ("-DASHED", "1"),
        ("GREETING", "hello big world"),
        ("SPACED", "  padded  "),
        ("QUOTED", "\"a\" 'b' $HOME `date` ; | & > <"),
        ("MULTILINE", "first\nsecond"),
        ("EQUALS", "a=b=c"),
        ("UNICODE", "héllo ☃"),
        ("EMPTY", ""),
    ];
    let environment = || entries.map(|(name, value)| format!("{name}={value}"));
    let listing: String = entries
        .iter()
        .map(|(name, value)| format!("{name}={value}\n"))
        .collect();

    for request in [
        ExecRequest::argv(["/usr/bin/env"]),
        ExecRequest::argv(["/usr/bin/env"]).with_cwd("/tmp"),
        ExecRequest::argv(["/opt/a=b/printenv"]),
        ExecRequest::argv(["/opt/a=b/printenv"]).with_cwd("/tmp"),
    ] {
        let output = output_of(&nvx, &sandbox_id, request.with_environment(environment()));
        assert_eq!(output.stdout, listing.as_bytes());
    }
    // A shell reads the same values.
    for (name, value) in entries {
        let output = output_of(
            &nvx,
            &sandbox_id,
            ExecRequest::command_line(format!("printenv {name}")).with_environment(environment()),
        );
        assert_eq!(output.stdout, format!("{value}\n").as_bytes(), "{name}");
    }
    nvx.stop(&sandbox_id).unwrap();
    nvx.deprovision(&sandbox_id).unwrap();
}

#[test]
fn each_execution_has_its_own_environment() {
    let fixture = Fixture::new();
    let nvx = fixture.nvx();
    let sandbox_id = nvx.provision(&ProvisionRequest::new()).unwrap().sandbox_id;
    nvx.start(&sandbox_id).unwrap();
    let foo = |request: ExecRequest| output_of(&nvx, &sandbox_id, request).stdout;
    let print = || ExecRequest::command_line("printenv FOO");

    assert_eq!(foo(print().with_env("FOO=one")), b"one\n");
    assert_eq!(foo(print().with_env("FOO=two")), b"two\n");
    // Nothing is left over once a later execution supplies no environment.
    assert_eq!(foo(print()), b"");
    assert_eq!(foo(print().with_environment(Vec::<String>::new())), b"");
    assert_eq!(foo(print().with_env("FOO=")), b"\n");
    assert_eq!(foo(print().with_env("FOO=one")), b"one\n");
    assert_eq!(foo(print()), b"");
    assert_eq!(
        environment_of(&nvx, &sandbox_id, ExecRequest::argv(["/usr/bin/env"])),
        sorted(DEFAULT_ENVIRONMENT)
    );

    // Overlapping executions are serialized and keep their own values.
    let slow = nvx
        .exec(
            &sandbox_id,
            &ExecRequest::command_line("sleep 200; printenv FOO").with_env("FOO=slow"),
        )
        .unwrap();
    let fast = nvx
        .exec(&sandbox_id, &print().with_env("FOO=fast"))
        .unwrap();
    assert_eq!(slow.wait_with_output().unwrap().stdout, b"slow\n");
    assert_eq!(fast.wait_with_output().unwrap().stdout, b"fast\n");
    nvx.stop(&sandbox_id).unwrap();
    nvx.deprovision(&sandbox_id).unwrap();
}

#[test]
fn programs_get_exactly_the_requested_environment_whatever_starts_them() {
    let fixture = Fixture::new();
    let nvx = fixture.nvx();
    let sandbox_id = nvx.provision(&ProvisionRequest::new()).unwrap().sandbox_id;
    nvx.start(&sandbox_id).unwrap();
    let environment = |request| environment_of(&nvx, &sandbox_id, request);
    let print = || ExecRequest::argv(["/usr/bin/env"]);
    // A shell exports `PWD`, `OLDPWD`, and `SHLVL`, and it rewrites entries with these names.
    let shell_names = [
        "PWD=/custom",
        "SHLVL=7",
        "OLDPWD=/keep",
        "-DASHED=1",
        "FOO=bar",
    ];

    // The shell that enters the working directory runs before `env`, so it leaves no trace.
    assert_eq!(
        environment(print().with_cwd("/work").with_env("FOO=bar")),
        sorted(["FOO=bar"])
    );
    assert_eq!(
        environment(
            print()
                .with_cwd("/work")
                .with_environment(Vec::<String>::new())
        ),
        Vec::<String>::new()
    );
    assert_eq!(
        environment(print().with_cwd("/work").with_environment(shell_names)),
        sorted(shell_names)
    );
    // Layering puts the entries on top of whatever that shell exported, so the entries win.
    let layered = environment(
        print()
            .with_cwd("/work")
            .with_environment(["PWD=/custom", "SHLVL=7", "FOO=bar"])
            .with_inherit_default_env(true),
    );
    for expected in [
        "PWD=/custom",
        "SHLVL=7",
        "FOO=bar",
        "PATH=/usr/sbin:/usr/bin:/sbin:/bin",
    ] {
        assert!(layered.iter().any(|line| line == expected), "{layered:?}");
    }

    // A program whose name contains `=` cannot start behind `env`, but no shell may replace it.
    let named = || ExecRequest::argv(["/opt/a=b/printenv"]);
    assert_eq!(
        environment(named().with_env("FOO=bar")),
        sorted(["FOO=bar"])
    );
    assert_eq!(
        environment(named().with_cwd("/work").with_env("FOO=bar")),
        sorted(["FOO=bar"])
    );
    assert_eq!(
        environment(named().with_environment(Vec::<String>::new())),
        Vec::<String>::new()
    );
    assert_eq!(
        environment(named().with_cwd("/work").with_environment(shell_names)),
        sorted(shell_names)
    );
    assert_eq!(
        environment(named().with_env("FOO=bar").with_inherit_default_env(true)),
        sorted(DEFAULT_ENVIRONMENT.into_iter().chain(["FOO=bar"]))
    );
    nvx.stop(&sandbox_id).unwrap();
    nvx.deprovision(&sandbox_id).unwrap();
}

#[test]
fn a_command_line_runs_in_a_shell_that_exports_variables_of_its_own() {
    let fixture = Fixture::new();
    let nvx = fixture.nvx();
    let sandbox_id = nvx.provision(&ProvisionRequest::new()).unwrap().sandbox_id;
    nvx.start(&sandbox_id).unwrap();
    let environment = |request| environment_of(&nvx, &sandbox_id, request);
    let print = || ExecRequest::command_line("env");

    // The shell is the workload, so it keeps the entries and adds `SHLVL` and `PWD`...
    assert_eq!(
        environment(print().with_env("FOO=bar")),
        sorted(["FOO=bar", "SHLVL=1", "PWD=/"])
    );
    // ...and `OLDPWD` once it has entered a working directory...
    assert_eq!(
        environment(print().with_cwd("/work").with_env("FOO=bar")),
        sorted(["FOO=bar", "SHLVL=1", "PWD=/work", "OLDPWD=/"])
    );
    // ...and it rewrites entries with those names, as every shell does.
    assert_eq!(
        environment(print().with_env("SHLVL=7")),
        sorted(["SHLVL=8", "PWD=/"])
    );
    nvx.stop(&sandbox_id).unwrap();
    nvx.deprovision(&sandbox_id).unwrap();
}

#[test]
fn environments_combine_with_working_directories_and_programs_with_equals_signs() {
    let fixture = Fixture::new();
    let nvx = fixture.nvx();
    let sandbox_id = nvx.provision(&ProvisionRequest::new()).unwrap().sandbox_id;
    nvx.start(&sandbox_id).unwrap();
    let output = output_of(
        &nvx,
        &sandbox_id,
        ExecRequest::command_line("pwd; printenv FOO")
            .with_cwd("/work")
            .with_env("FOO=bar baz"),
    );
    assert_eq!(output.stdout, b"/work\nbar baz\n");
    let output = output_of(
        &nvx,
        &sandbox_id,
        ExecRequest::argv(["/usr/bin/env"])
            .with_cwd("/work")
            .with_env("FOO=bar baz"),
    );
    assert_eq!(output.stdout, b"FOO=bar baz\n");

    // `env` would read the program name as one more entry and print the environment instead.
    for request in [
        ExecRequest::argv(["/opt/a=b/run", "arg"]).with_env("FOO=bar"),
        ExecRequest::argv(["/opt/a=b/run", "arg"]).with_environment(Vec::<String>::new()),
        ExecRequest::argv(["/opt/a=b/run", "arg"])
            .with_env("FOO=bar")
            .with_inherit_default_env(true),
        ExecRequest::argv(["/opt/a=b/run", "arg"])
            .with_cwd("/work")
            .with_env("FOO=bar"),
    ] {
        let output = nvx
            .exec(&sandbox_id, &request)
            .unwrap()
            .wait_with_output()
            .unwrap();
        assert_eq!(output.outcome, ExecOutcome::Exited(127), "{request:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("/opt/a=b/run: not found"),
            "{request:?}: {output:?}"
        );
        assert!(output.stdout.is_empty(), "{request:?}: {output:?}");
    }
    nvx.stop(&sandbox_id).unwrap();
    nvx.deprovision(&sandbox_id).unwrap();
}

#[test]
fn concurrent_execs_are_serialized() {
    let fixture = Fixture::new();
    let nvx = fixture.nvx();
    let sandbox_id = nvx.provision(&ProvisionRequest::new()).unwrap().sandbox_id;
    nvx.start(&sandbox_id).unwrap();
    let slow = nvx
        .exec(
            &sandbox_id,
            &ExecRequest::command_line("sleep 300; echo first"),
        )
        .unwrap();
    let fast = nvx
        .exec(&sandbox_id, &ExecRequest::command_line("echo second"))
        .unwrap();
    assert_eq!(slow.wait_with_output().unwrap().stdout, b"first\n");
    assert_eq!(fast.wait_with_output().unwrap().stdout, b"second\n");
    nvx.stop(&sandbox_id).unwrap();
    nvx.deprovision(&sandbox_id).unwrap();
}
