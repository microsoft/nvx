//! Opt-in WHP lifecycle and robustness proofs with a separately supplied native library and guest.
//!
//! The tests need `NVXHOST_TEST_OPENVMM`, `NVXHOST_TEST_KERNEL`, `NVXHOST_TEST_INITRD`,
//! `NVXHOST_TEST_IMAGE`, `NVXHOST_TEST_LIBRARY`, and the approved `NVXHOST_TEST_SHA256`. Run them
//! one at a time so that the check for leftover OpenVMM processes is exact, and optimized, since
//! every provision and start hashes the image:
//! `cargo test --release --features nvxhost --test nvxhost_guest -- --ignored --test-threads=1`.
#![cfg(feature = "nvxhost")]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use aci_edge_sandboxes::openvmm::{Hypervisor, NvxHostBackend, NvxHostConfig, OpenVmmConfig};
use aci_edge_sandboxes::{
    AciEdgeSandbox, Error, ErrorCode, ExecOutcome, ExecOutput, ExecRequest, ProvisionRequest,
    Result, SandboxId, StdinMode, StopResult,
};

const HELPER_STATE: &str = "NVXHOST_TEST_HELPER_STATE";
const HELPER_SANDBOX: &str = "NVXHOST_TEST_HELPER_SANDBOX";

fn required(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("{name} must be set")))
}

fn approved_digest() -> [u8; 32] {
    let hex = std::env::var("NVXHOST_TEST_SHA256").expect("NVXHOST_TEST_SHA256 must be set");
    assert_eq!(hex.len(), 64, "the approved digest must contain 64 digits");
    let mut digest = [0u8; 32];
    for (index, value) in digest.iter_mut().enumerate() {
        *value = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
            .expect("the approved digest must be hexadecimal");
    }
    digest
}

fn backend_with(
    state: &Path,
    guest_debug: bool,
    adjust: impl FnOnce(&mut OpenVmmConfig),
) -> Arc<NvxHostBackend> {
    let mut config = OpenVmmConfig::new(
        required("NVXHOST_TEST_OPENVMM"),
        required("NVXHOST_TEST_KERNEL"),
        required("NVXHOST_TEST_INITRD"),
        Hypervisor::Whp,
        state,
    );
    adjust(&mut config);
    Arc::new(
        NvxHostBackend::new(
            NvxHostConfig::new(
                config,
                required("NVXHOST_TEST_IMAGE"),
                required("NVXHOST_TEST_LIBRARY"),
                approved_digest(),
            )
            .with_guest_debug(guest_debug),
        )
        .unwrap(),
    )
}

fn backend(state: &Path) -> Arc<NvxHostBackend> {
    backend_with(state, false, |_| {})
}

/// Lists the OpenVMM processes running on this host, failing if they cannot be listed.
fn openvmm_processes() -> BTreeSet<u32> {
    let executable = required("NVXHOST_TEST_OPENVMM");
    let name = executable
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    #[cfg(windows)]
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("[Diagnostics.Process]::GetProcessesByName('{name}') | ForEach-Object Id"),
        ])
        .output()
        .unwrap();
    #[cfg(not(windows))]
    let output = Command::new("pgrep").args(["-x", &name]).output().unwrap();
    // pgrep exits with 1 when no process matches.
    assert!(
        output.status.success() || (cfg!(not(windows)) && output.status.code() == Some(1)),
        "cannot list OpenVMM processes: {output:?}"
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.parse()
                .unwrap_or_else(|_| panic!("unexpected process ID {line:?}"))
        })
        .collect()
}

/// Fails if an OpenVMM process that was not running before the test is still running.
fn assert_no_new_openvmm(before: &BTreeSet<u32>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let leaked: Vec<u32> = openvmm_processes().difference(before).copied().collect();
        if leaked.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "OpenVMM processes {leaked:?} outlived their sandboxes"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

/// Terminates a process abruptly, as a crash would.
fn kill_process(pid: u32) {
    #[cfg(windows)]
    let status = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("Stop-Process -Id {pid} -Force -ErrorAction Stop"),
        ])
        .status()
        .unwrap();
    #[cfg(not(windows))]
    let status = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "cannot kill process {pid}: {status}");
}

/// Stops and removes a sandbox when a test ends, printing its diagnostics if the test failed.
struct Cleanup<'a> {
    client: &'a AciEdgeSandbox,
    backend: &'a NvxHostBackend,
    id: SandboxId,
    armed: bool,
}

impl Cleanup<'_> {
    fn release(mut self) -> SandboxId {
        self.armed = false;
        self.id.clone()
    }
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if thread::panicking() {
            let read = |path: PathBuf| {
                std::fs::read_to_string(&path)
                    .unwrap_or_else(|error| format!("cannot read {}: {error}", path.display()))
            };
            eprintln!(
                "sandbox {}\nOpenVMM log: {}\nguest console: {}\nOpenVMM outcome: {}",
                self.id,
                read(self.backend.log_path(&self.id)),
                read(self.backend.console_log_path(&self.id)),
                read(self.backend.outcome_report_path(&self.id)),
            );
        }
        let _ = self.client.stop(&self.id);
        let _ = self.client.deprovision(&self.id);
    }
}

fn provision<'a>(client: &'a AciEdgeSandbox, backend: &'a NvxHostBackend) -> Cleanup<'a> {
    let id = client
        .provision(&ProvisionRequest::new())
        .unwrap()
        .sandbox_id;
    Cleanup {
        client,
        backend,
        id,
        armed: true,
    }
}

fn exec(client: &AciEdgeSandbox, id: &SandboxId, request: &ExecRequest) -> ExecOutput {
    client
        .exec(id, request)
        .and_then(|execution| execution.wait_with_output())
        .unwrap_or_else(|error| panic!("{request:?} failed: {error}"))
}

fn assert_prints(client: &AciEdgeSandbox, id: &SandboxId, text: &str) {
    let output = exec(
        client,
        id,
        &ExecRequest::command_line(format!("printf %s {text}")),
    );
    assert_eq!(output.outcome, ExecOutcome::Exited(0), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), text);
}

fn forced(stopped: &StopResult) -> Option<bool> {
    stopped
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("forced"))
        .and_then(serde_json::Value::as_bool)
}

/// Fails if the boot-console capture reported an error.
fn assert_console_captured(stopped: &StopResult) {
    assert!(
        stopped
            .metadata
            .as_ref()
            .is_none_or(|metadata| !metadata.contains_key("consoleError")),
        "{:?}",
        stopped.metadata
    );
}

fn assert_graceful(stopped: Result<StopResult>) {
    let stopped = stopped.unwrap();
    assert_eq!(forced(&stopped), Some(false), "{:?}", stopped.metadata);
    assert_console_captured(&stopped);
}

fn failure<T>(result: Result<T>) -> ErrorCode {
    match result {
        Ok(_) => panic!("the operation unexpectedly succeeded"),
        Err(error) => error.code(),
    }
}

fn start_in_helper(state: &Path, id: &SandboxId) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args([
            "helper_starts_a_sandbox_in_another_process",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HELPER_STATE, state)
        .env(HELPER_SANDBOX, id.as_str())
        .stdin(Stdio::null())
        .spawn()
        .unwrap()
}

#[test]
#[ignore = "requires an approved private DLL, edge initramfs, GPT image, and a WHP host"]
fn whp_guest_lifecycle_stops_gracefully_and_cleans_up() {
    let state = tempfile::tempdir().unwrap();
    let config = OpenVmmConfig::new(
        required("NVXHOST_TEST_OPENVMM"),
        required("NVXHOST_TEST_KERNEL"),
        required("NVXHOST_TEST_INITRD"),
        Hypervisor::Whp,
        state.path(),
    );
    let backend = Arc::new(
        NvxHostBackend::new(
            NvxHostConfig::new(
                config,
                required("NVXHOST_TEST_IMAGE"),
                required("NVXHOST_TEST_LIBRARY"),
                approved_digest(),
            )
            .with_guest_debug(true),
        )
        .unwrap(),
    );
    let client = AciEdgeSandbox::from_shared(backend.clone());
    let sandbox_id = client
        .provision(&ProvisionRequest::new())
        .unwrap()
        .sandbox_id;
    let result = (|| {
        let started = client.start(&sandbox_id)?;
        if !started
            .metadata
            .as_ref()
            .is_some_and(|metadata| metadata.contains_key("guestBuildId"))
        {
            return Err(Error::new(
                ErrorCode::BackendError,
                "the guest did not report a build ID",
            ));
        }
        let executed = client
            .exec(&sandbox_id, &ExecRequest::command_line("printf READY"))?
            .wait_with_output()?;
        if executed.outcome != ExecOutcome::Exited(0) || executed.stdout != b"READY" {
            return Err(Error::new(
                ErrorCode::BackendError,
                format!(
                    "guest exec outcome {:?}, stdout {:?}, stderr {:?}",
                    executed.outcome,
                    String::from_utf8_lossy(&executed.stdout),
                    String::from_utf8_lossy(&executed.stderr),
                ),
            ));
        }
        let logs = backend.guest_logs(&sandbox_id)?;
        if !logs.windows(8).any(|window| window == b"execute:") {
            return Err(Error::new(
                ErrorCode::BackendError,
                "guest log stream omitted the executed command",
            ));
        }
        let stopped = client.stop(&sandbox_id)?;
        if stopped
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("forced"))
            .and_then(serde_json::Value::as_bool)
            != Some(false)
        {
            return Err(Error::new(
                ErrorCode::BackendError,
                format!("guest shutdown was not graceful: {:?}", stopped.metadata),
            ));
        }
        let console = std::fs::read(backend.console_log_path(&sandbox_id)).map_err(|error| {
            Error::new(
                ErrorCode::BackendError,
                "cannot read the guest boot console",
            )
            .with_source(error)
        })?;
        if [b"NVX-EDGE-FATAL".as_slice(), b"NVX-EDGE-SHUTDOWN-ERROR"]
            .iter()
            .any(|marker| {
                console
                    .windows(marker.len())
                    .any(|window| window == *marker)
            })
        {
            return Err(Error::new(
                ErrorCode::BackendError,
                "the guest reported a fatal or shutdown error",
            ));
        }
        client.deprovision(&sandbox_id)?;
        if state
            .path()
            .join("nvxhost")
            .join(sandbox_id.token())
            .exists()
        {
            return Err(Error::new(
                ErrorCode::BackendError,
                "deprovision left sandbox state behind",
            ));
        }
        Ok::<_, aci_edge_sandboxes::Error>(())
    })();
    if let Err(error) = result {
        let log = std::fs::read_to_string(backend.log_path(&sandbox_id))
            .unwrap_or_else(|read| format!("cannot read OpenVMM diagnostic log: {read}"));
        let console = std::fs::read_to_string(backend.console_log_path(&sandbox_id))
            .unwrap_or_else(|read| format!("cannot read guest boot console: {read}"));
        let outcome = std::fs::read_to_string(backend.outcome_report_path(&sandbox_id))
            .unwrap_or_else(|read| format!("cannot read OpenVMM outcome report: {read}"));
        let stop = client.stop(&sandbox_id);
        let deprovision = client.deprovision(&sandbox_id);
        panic!(
            "WHP lifecycle failed: {error}; recovery stop: {stop:?}; \
             recovery deprovision: {deprovision:?}; OpenVMM log: {log}; \
             guest console: {console}; OpenVMM outcome: {outcome}"
        );
    }
}

#[test]
#[ignore = "requires an approved private DLL, edge initramfs, GPT image, and a WHP host"]
fn whp_exec_reports_exit_codes_streams_timeouts_and_output_limits() {
    let state = tempfile::tempdir().unwrap();
    let before = openvmm_processes();
    let backend = backend(state.path());
    let client = AciEdgeSandbox::from_shared(backend.clone());
    let sandbox = provision(&client, &backend);
    let id = &sandbox.id;
    client.start(id).unwrap();

    let output = exec(
        &client,
        id,
        &ExecRequest::command_line("printf out; printf err >&2; exit 7"),
    );
    assert_eq!(output.outcome, ExecOutcome::Exited(7));
    assert_eq!(output.stdout, b"out");
    assert_eq!(output.stderr, b"err");

    let output = exec(
        &client,
        id,
        &ExecRequest::argv([
            "/bin/sh",
            "-c",
            "printf '%s|' \"$@\"",
            "sh",
            "a  b",
            "$HOME",
        ]),
    );
    assert_eq!(output.outcome, ExecOutcome::Exited(0));
    assert_eq!(output.stdout, b"a  b|$HOME|");

    let started = Instant::now();
    let output = exec(
        &client,
        id,
        &ExecRequest::command_line("sleep 30").with_timeout(Duration::from_secs(1)),
    );
    assert_eq!(output.outcome, ExecOutcome::TimedOut);
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "a one-second timeout took {:?}",
        started.elapsed()
    );

    let error = client
        .exec(id, &ExecRequest::command_line("yes | head -c 1100000"))
        .and_then(|execution| execution.wait_with_output())
        .expect_err("output above the guest limit must fail rather than truncate");
    assert_eq!(error.code(), ErrorCode::BackendError, "{error}");
    assert!(error.message().contains("status 8"), "{error}");
    assert_prints(&client, id, "after-overflow");

    for index in 0..20 {
        assert_prints(&client, id, &format!("command{index}"));
    }
    for request in [
        ExecRequest::command_line("pwd").with_cwd("/tmp"),
        ExecRequest::command_line("env").with_env("NAME=value"),
        ExecRequest::command_line("cat").with_stdin(StdinMode::Piped),
    ] {
        assert_eq!(
            failure(client.exec(id, &request)),
            ErrorCode::PolicyValidation
        );
    }
    let logs = backend.guest_logs(id).unwrap();
    assert!(logs.windows(8).any(|window| window == b"execute:"));
    assert!(logs.windows(13).any(|window| window == b"output-limit:"));

    assert_graceful(client.stop(id));
    client.deprovision(id).unwrap();
    assert_no_new_openvmm(&before);
}

#[test]
#[ignore = "requires an approved private DLL, edge initramfs, GPT image, and a WHP host"]
fn whp_lifecycle_errors_follow_state_and_a_stopped_sandbox_restarts() {
    let state = tempfile::tempdir().unwrap();
    let before = openvmm_processes();
    let backend = backend(state.path());
    let client = AciEdgeSandbox::from_shared(backend.clone());
    let sandbox = provision(&client, &backend);
    let id = &sandbox.id;

    assert_eq!(
        failure(client.exec(id, &ExecRequest::command_line("true"))),
        ErrorCode::NotStarted
    );
    client.start(id).unwrap();
    assert_eq!(failure(client.start(id)), ErrorCode::AlreadyStarted);
    assert_eq!(failure(client.deprovision(id)), ErrorCode::AlreadyStarted);
    assert_prints(&client, id, "first");
    assert_graceful(client.stop(id));
    assert_eq!(failure(client.stop(id)), ErrorCode::AlreadyStopped);
    assert_eq!(
        failure(client.exec(id, &ExecRequest::command_line("true"))),
        ErrorCode::NotStarted
    );

    client.start(id).unwrap();
    assert_prints(&client, id, "second");
    assert_graceful(client.stop(id));
    client.deprovision(id).unwrap();
    assert_eq!(failure(client.start(id)), ErrorCode::StaleId);
    assert_no_new_openvmm(&before);
}

#[test]
#[ignore = "requires an approved private DLL, edge initramfs, GPT image, and a WHP host"]
fn whp_new_backend_reattaches_to_a_running_guest_and_forces_a_stop() {
    let state = tempfile::tempdir().unwrap();
    let before = openvmm_processes();
    let id = {
        let first = backend(state.path());
        let client = AciEdgeSandbox::from_shared(first.clone());
        let sandbox = provision(&client, &first);
        client.start(&sandbox.id).unwrap();
        sandbox.release()
    };

    let second = backend_with(state.path(), false, |config| {
        config.stop_timeout = Duration::from_millis(1);
    });
    let client = AciEdgeSandbox::from_shared(second.clone());
    let sandbox = Cleanup {
        client: &client,
        backend: &second,
        id,
        armed: true,
    };
    assert_prints(&client, &sandbox.id, "reattached");
    let logs = second.guest_logs(&sandbox.id).unwrap();
    assert!(logs.windows(8).any(|window| window == b"execute:"));
    let stopped = client.stop(&sandbox.id).unwrap();
    assert_eq!(forced(&stopped), Some(true), "{:?}", stopped.metadata);
    // The first backend's listener still held the boot console, so the second one waited.
    assert_console_captured(&stopped);
    assert_eq!(
        failure(client.exec(&sandbox.id, &ExecRequest::command_line("true"))),
        ErrorCode::NotStarted
    );
    client.deprovision(&sandbox.id).unwrap();
    assert_no_new_openvmm(&before);
}

#[test]
#[ignore = "requires an approved private DLL, edge initramfs, GPT image, and a WHP host"]
fn whp_a_crashed_guest_is_not_running_and_restarts_with_its_console_captured() {
    let state = tempfile::tempdir().unwrap();
    let before = openvmm_processes();
    let backend = backend_with(state.path(), true, |_| {});
    let client = AciEdgeSandbox::from_shared(backend.clone());
    let sandbox = provision(&client, &backend);
    let id = &sandbox.id;
    client.start(id).unwrap();
    assert_prints(&client, id, "before-crash");

    let launched: Vec<u32> = openvmm_processes().difference(&before).copied().collect();
    assert_eq!(launched.len(), 1, "{launched:?}");
    kill_process(launched[0]);
    assert_no_new_openvmm(&before);
    assert_eq!(
        failure(client.exec(id, &ExecRequest::command_line("true"))),
        ErrorCode::NotStarted
    );
    assert_eq!(failure(client.stop(id)), ErrorCode::AlreadyStopped);

    let console = backend.console_log_path(id);
    let crashed = std::fs::metadata(&console).unwrap().len();
    client.start(id).unwrap();
    assert_prints(&client, id, "after-crash");
    assert_graceful(client.stop(id));
    let restarted = std::fs::metadata(&console).unwrap().len();
    assert!(
        restarted > crashed,
        "the restarted guest's boot console was not captured ({crashed} -> {restarted} bytes)"
    );
    client.deprovision(id).unwrap();
    assert_no_new_openvmm(&before);
}

#[test]
#[ignore = "helper process for whp_guest_survives_its_caller_and_a_killed_start_leaves_no_vm"]
fn helper_starts_a_sandbox_in_another_process() {
    let Some(sandbox) = std::env::var_os(HELPER_SANDBOX) else {
        return;
    };
    let client = AciEdgeSandbox::from_shared(backend(&required(HELPER_STATE)));
    let id = SandboxId::parse(&sandbox.to_string_lossy()).unwrap();
    client.start(&id).unwrap();
}

#[test]
#[ignore = "requires an approved private DLL, edge initramfs, GPT image, and a WHP host"]
fn whp_guest_survives_its_caller_and_a_killed_start_leaves_no_vm() {
    let state = tempfile::tempdir().unwrap();
    let before = openvmm_processes();
    let backend = backend_with(state.path(), false, |config| {
        config.stop_timeout = Duration::from_secs(5);
    });
    let client = AciEdgeSandbox::from_shared(backend.clone());

    let sandbox = provision(&client, &backend);
    let status = start_in_helper(state.path(), &sandbox.id).wait().unwrap();
    assert!(
        status.success(),
        "the helper process could not start the guest: {status}"
    );
    assert_prints(&client, &sandbox.id, "survived");
    assert_graceful(client.stop(&sandbox.id));
    client.deprovision(&sandbox.id).unwrap();
    drop(sandbox);
    assert_no_new_openvmm(&before);

    for delay in [0, 100, 300, 700, 1500] {
        let sandbox = provision(&client, &backend);
        let mut helper = start_in_helper(state.path(), &sandbox.id);
        thread::sleep(Duration::from_millis(delay));
        let _ = helper.kill();
        let _ = helper.wait();
        match client.stop(&sandbox.id) {
            Ok(_) => {}
            Err(error) if error.code() == ErrorCode::AlreadyStopped => {}
            Err(error) => panic!("stop after a start killed at {delay} ms failed: {error}"),
        }
        client.deprovision(&sandbox.id).unwrap_or_else(|error| {
            panic!("deprovision after a start killed at {delay} ms failed: {error}")
        });
        drop(sandbox);
        assert_no_new_openvmm(&before);
    }
}

#[test]
#[ignore = "requires an approved private DLL, edge initramfs, GPT image, and a WHP host"]
fn whp_parallel_sandboxes_and_repeated_restarts_stay_isolated() {
    let state = tempfile::tempdir().unwrap();
    let before = openvmm_processes();
    let backend = backend(state.path());
    let client = AciEdgeSandbox::from_shared(backend.clone());

    thread::scope(|scope| {
        let workers: Vec<_> = (0..4)
            .map(|index| {
                let (client, backend) = (&client, &backend);
                scope.spawn(move || {
                    let sandbox = provision(client, backend);
                    client.start(&sandbox.id).unwrap();
                    assert_prints(client, &sandbox.id, &format!("sandbox{index}"));
                    assert_graceful(client.stop(&sandbox.id));
                    client.deprovision(&sandbox.id).unwrap();
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
    });
    assert_no_new_openvmm(&before);

    let sandbox = provision(&client, &backend);
    let mut starts = Vec::new();
    for cycle in 0..8 {
        let called = Instant::now();
        let started = client.start(&sandbox.id).unwrap();
        let total = called.elapsed().as_millis();
        let boot = started
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("bootMilliseconds"))
            .and_then(serde_json::Value::as_u64)
            .unwrap();
        starts.push((total, boot));
        assert_prints(&client, &sandbox.id, &format!("cycle{cycle}"));
        assert_graceful(client.stop(&sandbox.id));
    }
    client.deprovision(&sandbox.id).unwrap();
    eprintln!("(start call, boot) milliseconds across restarts: {starts:?}");
    assert_no_new_openvmm(&before);
}

#[test]
#[ignore = "requires an approved private DLL, edge initramfs, GPT image, and a WHP host"]
fn whp_concurrent_commands_share_one_guest() {
    let state = tempfile::tempdir().unwrap();
    let before = openvmm_processes();
    let backend = backend(state.path());
    let client = AciEdgeSandbox::from_shared(backend.clone());
    let sandbox = provision(&client, &backend);
    client.start(&sandbox.id).unwrap();

    let started = Instant::now();
    thread::scope(|scope| {
        let workers: Vec<_> = (0..4)
            .map(|index| {
                let (client, id) = (&client, &sandbox.id);
                scope.spawn(move || {
                    let output = exec(
                        client,
                        id,
                        &ExecRequest::command_line(format!("sleep 1; printf parallel{index}")),
                    );
                    assert_eq!(output.outcome, ExecOutcome::Exited(0), "{output:?}");
                    assert_eq!(
                        String::from_utf8_lossy(&output.stdout),
                        format!("parallel{index}")
                    );
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
    });
    eprintln!(
        "four concurrent one-second commands took {:?}",
        started.elapsed()
    );
    assert_graceful(client.stop(&sandbox.id));
    client.deprovision(&sandbox.id).unwrap();
    assert_no_new_openvmm(&before);
}
