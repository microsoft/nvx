//! Opt-in lifecycle, robustness, host-path, and network proofs with a separately supplied native
//! library and guest.
//!
//! The tests need `NVXHOST_TEST_OPENVMM`, `NVXHOST_TEST_KERNEL`, `NVXHOST_TEST_INITRD`,
//! `NVXHOST_TEST_IMAGE`, `NVXHOST_TEST_LIBRARY`, and the approved `NVXHOST_TEST_SHA256`.
//! `NVXHOST_TEST_HYPERVISOR` selects `whp`, `mshv`, or `kvm`, and defaults to `whp` on Windows.
//! The host-path and network tests also need `python3` in the image. Run the tests one at a time
//! so that the check for leftover OpenVMM processes is exact, and optimized, since creating a
//! backend hashes the runtime files and registering an image hashes the image:
//! `cargo test --release --features nvxhost --test nvxhost_guest -- --ignored --test-threads=1`.
#![cfg(feature = "nvxhost")]

use std::collections::BTreeSet;
use std::io::{ErrorKind, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use aci_edge_sandboxes::openvmm::{
    Hypervisor, ImageDigest, ImageId, NvxHostBackend, NvxHostConfig, OpenVmmConfig,
    resolve_guest_path,
};
use aci_edge_sandboxes::{
    Access, AciEdgeSandbox, EgressPolicy, Error, ErrorCode, ExecOutcome, ExecOutput, ExecRequest,
    FilesystemPolicy, NetworkPolicy, NetworkRule, Protocol, ProvisionRequest, Result, SandboxId,
    StdinMode, StopResult,
};

const HELPER_STATE: &str = "NVXHOST_TEST_HELPER_STATE";
const HELPER_SANDBOX: &str = "NVXHOST_TEST_HELPER_SANDBOX";
/// Hashing the test image took most of a second of every start before images were registered.
const MAX_START_OVERHEAD: Duration = Duration::from_millis(500);

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

/// Returns the hypervisor that `NVXHOST_TEST_HYPERVISOR` names, which defaults to WHP on Windows.
fn hypervisor() -> Hypervisor {
    match std::env::var("NVXHOST_TEST_HYPERVISOR") {
        Ok(name) => name
            .parse()
            .unwrap_or_else(|error| panic!("NVXHOST_TEST_HYPERVISOR: {error}")),
        Err(_) if cfg!(windows) => Hypervisor::Whp,
        Err(_) => panic!("NVXHOST_TEST_HYPERVISOR must name the hypervisor, such as mshv"),
    }
}

/// Returns the test configuration for `image`, with sandbox state under `state`.
fn native_config(state: &Path, image: &Path) -> NvxHostConfig {
    NvxHostConfig::new(
        OpenVmmConfig::new(
            required("NVXHOST_TEST_OPENVMM"),
            required("NVXHOST_TEST_KERNEL"),
            required("NVXHOST_TEST_INITRD"),
            hypervisor(),
            state,
        ),
        image,
        required("NVXHOST_TEST_LIBRARY"),
        approved_digest(),
    )
}

fn backend_with(
    state: &Path,
    guest_debug: bool,
    adjust: impl FnOnce(&mut OpenVmmConfig),
) -> Arc<NvxHostBackend> {
    let mut config =
        native_config(state, &required("NVXHOST_TEST_IMAGE")).with_guest_debug(guest_debug);
    adjust(&mut config.openvmm);
    Arc::new(NvxHostBackend::new(config).unwrap())
}

fn backend(state: &Path) -> Arc<NvxHostBackend> {
    backend_with(state, false, |_| {})
}

/// Copies the test image into `state`, so that a test can change the copy.
fn copied_image(state: &Path) -> PathBuf {
    let image = state.join("image.vhd");
    std::fs::copy(required("NVXHOST_TEST_IMAGE"), &image).unwrap();
    image
}

/// Moves a file's modification time without changing its content.
fn touch(path: &Path) {
    let file = std::fs::File::options().write(true).open(path).unwrap();
    let modified = file.metadata().unwrap().modified().unwrap();
    file.set_modified(modified + Duration::from_secs(2))
        .unwrap();
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
    provision_with(client, backend, &ProvisionRequest::new())
}

fn provision_with<'a>(
    client: &'a AciEdgeSandbox,
    backend: &'a NvxHostBackend,
    request: &ProvisionRequest,
) -> Cleanup<'a> {
    let id = client.provision(request).unwrap().sandbox_id;
    Cleanup {
        client,
        backend,
        id,
        armed: true,
    }
}

/// Provisions a sandbox for `request` and starts it.
fn started<'a>(
    client: &'a AciEdgeSandbox,
    backend: &'a NvxHostBackend,
    request: &ProvisionRequest,
) -> Cleanup<'a> {
    let sandbox = provision_with(client, backend, request);
    client.start(&sandbox.id).unwrap();
    sandbox
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

/// Runs `argv` in the guest without a shell, so that paths need no quoting.
fn run(client: &AciEdgeSandbox, id: &SandboxId, argv: &[&str]) -> ExecOutput {
    exec(client, id, &ExecRequest::argv(argv.iter().copied()))
}

/// Runs a Python program with `args` in the guest and returns what it printed.
fn python(client: &AciEdgeSandbox, id: &SandboxId, program: &str, args: &[&str]) -> String {
    let mut argv = vec!["/bin/sh", "-c", "exec python3 -c \"$@\"", "sh", program];
    argv.extend_from_slice(args);
    let output = exec(
        client,
        id,
        &ExecRequest::argv(argv).with_timeout(Duration::from_secs(120)),
    );
    assert_eq!(output.outcome, ExecOutcome::Exited(0), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

/// The guest path of an existing host path, which mappings derive from the resolved path.
fn guest(path: &Path) -> String {
    resolve_guest_path(path).unwrap()
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

/// Fails unless `result` is a `BackendUnavailable` error whose message contains `expected`.
fn assert_unavailable<T>(result: Result<T>, expected: &str) {
    match result {
        Ok(_) => {
            panic!("the operation unexpectedly succeeded instead of failing with {expected:?}")
        }
        Err(error) => {
            assert_eq!(error.code(), ErrorCode::BackendUnavailable, "{error}");
            assert!(error.message().contains(expected), "{error}");
        }
    }
}

/// Starts a sandbox and returns the time that the call spent outside the guest boot.
fn start_overhead(client: &AciEdgeSandbox, id: &SandboxId) -> Duration {
    let called = Instant::now();
    let started = client.start(id).unwrap();
    let total = called.elapsed();
    let boot = started
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("bootMilliseconds"))
        .and_then(serde_json::Value::as_u64)
        .unwrap();
    total.saturating_sub(Duration::from_millis(boot))
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
#[ignore = "requires an approved private library, edge initramfs, GPT image, and a hypervisor host"]
fn guest_lifecycle_stops_gracefully_and_cleans_up() {
    let state = tempfile::tempdir().unwrap();
    let config = OpenVmmConfig::new(
        required("NVXHOST_TEST_OPENVMM"),
        required("NVXHOST_TEST_KERNEL"),
        required("NVXHOST_TEST_INITRD"),
        hypervisor(),
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
            "guest lifecycle failed: {error}; recovery stop: {stop:?}; \
             recovery deprovision: {deprovision:?}; OpenVMM log: {log}; \
             guest console: {console}; OpenVMM outcome: {outcome}"
        );
    }
}

#[test]
#[ignore = "requires an approved private library, edge initramfs, GPT image, and a hypervisor host"]
fn exec_reports_exit_codes_streams_timeouts_and_output_limits() {
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
#[ignore = "requires an approved private library, edge initramfs, GPT image, and a hypervisor host"]
fn lifecycle_errors_follow_state_and_a_stopped_sandbox_restarts() {
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
#[ignore = "requires an approved private library, edge initramfs, GPT image, and a hypervisor host"]
fn new_backend_reattaches_to_a_running_guest_and_forces_a_stop() {
    let state = tempfile::tempdir().unwrap();
    let before = openvmm_processes();
    let id = {
        let first = backend(state.path());
        let client = AciEdgeSandbox::from_shared(first.clone());
        let sandbox = provision(&client, &first);
        client.start(&sandbox.id).unwrap();
        sandbox.release()
    };

    // No graceful shutdown fits in a nanosecond, so the stop has to terminate OpenVMM.
    let second = backend_with(state.path(), false, |config| {
        config.stop_timeout = Duration::from_nanos(1);
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
#[ignore = "requires an approved private library, edge initramfs, GPT image, and a hypervisor host"]
fn a_crashed_guest_is_not_running_and_restarts_with_its_console_captured() {
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
#[ignore = "helper process for guest_survives_its_caller_and_a_killed_start_leaves_no_vm"]
fn helper_starts_a_sandbox_in_another_process() {
    let Some(sandbox) = std::env::var_os(HELPER_SANDBOX) else {
        return;
    };
    let client = AciEdgeSandbox::from_shared(backend(&required(HELPER_STATE)));
    let id = SandboxId::parse(&sandbox.to_string_lossy()).unwrap();
    client.start(&id).unwrap();
}

#[test]
#[ignore = "requires an approved private library, edge initramfs, GPT image, and a hypervisor host"]
fn guest_survives_its_caller_and_a_killed_start_leaves_no_vm() {
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
#[ignore = "requires an approved private library, edge initramfs, GPT image, and a hypervisor host"]
fn parallel_sandboxes_and_repeated_restarts_stay_isolated() {
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
    assert!(
        starts
            .iter()
            .all(|&(total, boot)| total.saturating_sub(u128::from(boot))
                < MAX_START_OVERHEAD.as_millis()),
        "starts spent more than {MAX_START_OVERHEAD:?} outside the guest boot: {starts:?}"
    );
    assert_no_new_openvmm(&before);
}

#[test]
#[ignore = "requires an approved private library, edge initramfs, GPT image, and a hypervisor host"]
fn concurrent_commands_share_one_guest() {
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

#[test]
#[ignore = "requires an approved private library, edge initramfs, GPT image, and a hypervisor host"]
fn registered_images_start_without_hashing_and_fail_closed_after_a_change() {
    let state = tempfile::tempdir().unwrap();
    let before = openvmm_processes();
    let image = copied_image(state.path());
    let created = Instant::now();
    let first = NvxHostBackend::new(native_config(state.path(), &image)).unwrap();
    let hashed = created.elapsed();
    let id = first.image_id();
    let created = Instant::now();
    let backend = Arc::new(NvxHostBackend::new(native_config(state.path(), &image)).unwrap());
    let reused = created.elapsed();
    eprintln!("backend creation: {hashed:?} registering the image, {reused:?} reusing it");
    assert_eq!(backend.image_id(), id);
    assert_eq!(backend.runtime_digests(), first.runtime_digests());
    let images = backend.images().unwrap();
    assert_eq!(images.len(), 1, "{images:?}");
    assert_eq!(
        (
            images[0].id,
            &images[0].path,
            images[0].intact,
            images[0].trusted
        ),
        (id, &image, true, false)
    );

    let client = AciEdgeSandbox::from_shared(backend.clone());
    let sandbox = provision(&client, &backend);
    let overhead = start_overhead(&client, &sandbox.id);
    eprintln!("the first start spent {overhead:?} outside the guest boot");
    assert_prints(&client, &sandbox.id, "registered");
    assert_graceful(client.stop(&sandbox.id));

    // A changed seal fails closed, without hashing, until the image is registered again.
    touch(&image);
    assert_unavailable(
        client.start(&sandbox.id),
        "changed since it was registered; register it again",
    );
    assert_unavailable(client.provision(&ProvisionRequest::new()), "changed since");
    assert!(!backend.images().unwrap()[0].intact);
    assert_eq!(
        failure(backend.unregister_image(&id)),
        ErrorCode::PolicyValidation
    );
    let registered = backend
        .register_image(&image, ImageDigest::Compute)
        .unwrap();
    assert_eq!((registered.id, registered.intact), (id, true));
    client.start(&sandbox.id).unwrap();
    assert_prints(&client, &sandbox.id, "reregistered");
    assert_graceful(client.stop(&sandbox.id));
    backend.verify_image(&id).unwrap();

    client.deprovision(&sandbox.release()).unwrap();
    assert!(backend.unregister_image(&id).unwrap());
    assert!(!backend.unregister_image(&id).unwrap());
    assert!(backend.images().unwrap().is_empty());
    assert_unavailable(
        client.provision(&ProvisionRequest::new()),
        "no longer registered",
    );
    assert!(image.is_file());
    assert_no_new_openvmm(&before);
}

#[test]
#[ignore = "requires an approved private library, edge initramfs, GPT image, and a hypervisor host"]
fn trusted_digests_and_content_verification_follow_their_contracts() {
    let state = tempfile::tempdir().unwrap();
    let before = openvmm_processes();
    let image = copied_image(state.path());

    // A trusted digest is recorded without hashing, even one that the content does not have.
    let claimed = ImageId::from_sha256([7; 32]);
    let trusting = NvxHostBackend::new(
        native_config(state.path(), &image)
            .with_image_digest(ImageDigest::Trusted(*claimed.sha256())),
    )
    .unwrap();
    assert_eq!(trusting.image_id(), claimed);
    assert!(trusting.images().unwrap()[0].trusted);
    assert_unavailable(
        trusting.verify_image(&claimed),
        "no longer matches its SHA-256",
    );

    // Content verification hashes before every start, so it refuses the misattributed image.
    let verifying = Arc::new(
        NvxHostBackend::new(native_config(state.path(), &image).with_content_verification(true))
            .unwrap(),
    );
    assert_eq!(verifying.image_id(), claimed);
    let client = AciEdgeSandbox::from_shared(verifying.clone());
    let sandbox = provision(&client, &verifying);
    assert_unavailable(client.start(&sandbox.id), "no longer matches its SHA-256");

    // Runtime files are checked against approved digests and pinned per sandbox.
    let digests = verifying.runtime_digests();
    NvxHostBackend::new(native_config(state.path(), &image).with_runtime_digests(digests)).unwrap();
    let mut unapproved = digests;
    unapproved.initrd = [0; 32];
    assert_unavailable(
        NvxHostBackend::new(native_config(state.path(), &image).with_runtime_digests(unapproved)),
        "does not match the approved SHA-256",
    );
    let initrd = state.path().join("initramfs.cpio.gz");
    let mut changed = std::fs::read(required("NVXHOST_TEST_INITRD")).unwrap();
    changed.push(0);
    std::fs::write(&initrd, changed).unwrap();
    let mut config = native_config(state.path(), &image);
    config.openvmm.initrd = initrd;
    let other = AciEdgeSandbox::from_shared(Arc::new(NvxHostBackend::new(config).unwrap()));
    assert_unavailable(
        other.start(&sandbox.id),
        "provisioned with different OpenVMM runtime files",
    );
    client.deprovision(&sandbox.release()).unwrap();

    // Once the image is registered by its actual digest, a verified start succeeds.
    assert!(verifying.unregister_image(&claimed).unwrap());
    let actual = verifying
        .register_image(&image, ImageDigest::Compute)
        .unwrap()
        .id;
    assert_ne!(actual, claimed);
    let verifying = Arc::new(
        NvxHostBackend::new(native_config(state.path(), &image).with_content_verification(true))
            .unwrap(),
    );
    assert_eq!(verifying.image_id(), actual);
    let client = AciEdgeSandbox::from_shared(verifying.clone());
    let sandbox = provision(&client, &verifying);
    let overhead = start_overhead(&client, &sandbox.id);
    eprintln!("a start with content verification spent {overhead:?} outside the guest boot");
    assert_prints(&client, &sandbox.id, "verified");
    assert_graceful(client.stop(&sandbox.id));
    client.deprovision(&sandbox.release()).unwrap();
    assert_no_new_openvmm(&before);
}

#[test]
#[ignore = "requires an approved private library, edge initramfs, GPT image, and a hypervisor host"]
fn host_paths_follow_the_filesystem_policy() {
    let state = tempfile::tempdir().unwrap();
    let host = tempfile::tempdir().unwrap();
    let before = openvmm_processes();
    let work = host.path().join("work");
    for directory in ["src/secret", "out/frozen", "other"] {
        std::fs::create_dir_all(work.join(directory)).unwrap();
    }
    for (file, content) in [
        ("src/a.txt", "source"),
        ("src/secret/key", "hidden"),
        ("out/frozen/b.txt", "frozen"),
        ("config.json", "{}"),
        ("other/c.txt", "other"),
    ] {
        std::fs::write(work.join(file), content).unwrap();
    }
    let (src, out, frozen, config) = (
        work.join("src"),
        work.join("out"),
        work.join("out").join("frozen"),
        work.join("config.json"),
    );
    let request = ProvisionRequest::new().with_filesystem(FilesystemPolicy {
        // A read-only file inside the read-only src, which OpenVMM exports read-write for out,
        // and a read-only directory inside the read-write out, which only the guest protects.
        readonly_paths: vec![
            src.clone(),
            config.clone(),
            src.join("a.txt"),
            frozen.clone(),
        ],
        readwrite_paths: vec![out.clone()],
        denied_paths: vec![src.join("secret")],
    });
    let backend = backend(state.path());
    let client = AciEdgeSandbox::from_shared(backend.clone());
    // Workloads never see the sandbox state, which holds the plan of every sandbox: a mapping
    // inside the state root fails, and one that contains it needs a denied path that hides it.
    let exposing = |path: &Path, denied: Vec<PathBuf>| {
        ProvisionRequest::new().with_filesystem(FilesystemPolicy {
            readonly_paths: vec![path.to_path_buf()],
            denied_paths: denied,
            ..FilesystemPolicy::default()
        })
    };
    let parent = state.path().parent().unwrap();
    for request in [
        exposing(state.path(), Vec::new()),
        exposing(parent, Vec::new()),
    ] {
        assert_eq!(
            failure(client.provision(&request)),
            ErrorCode::PolicyValidation
        );
    }
    let hidden = client
        .provision(&exposing(parent, vec![state.path().to_path_buf()]))
        .unwrap();
    client.deprovision(&hidden.sandbox_id).unwrap();

    let sandbox = started(&client, &backend, &request);
    let id = &sandbox.id;
    let (guest_src, guest_out, guest_frozen, guest_config) =
        (guest(&src), guest(&out), guest(&frozen), guest(&config));

    let read = run(
        &client,
        id,
        &[
            "/bin/cat",
            &format!("{guest_src}/a.txt"),
            &guest_config,
            &format!("{guest_frozen}/b.txt"),
        ],
    );
    assert_eq!(read.stdout, b"source{}frozen", "{read:?}");
    let listing = run(&client, id, &["/bin/ls", "-a", &guest_src]);
    assert_eq!(listing.stdout, b".\n..\na.txt\n", "{listing:?}");
    let attempts: [[&str; 2]; 7] = [
        ["/bin/cat", &format!("{guest_src}/secret/key")],
        ["/bin/touch", &format!("{guest_src}/new")],
        ["/bin/touch", &guest_config],
        ["/bin/touch", &format!("{guest_frozen}/new")],
        ["/bin/rm", &format!("{guest_frozen}/b.txt")],
        ["/bin/ls", "/run/nvx/hostfs"],
        ["/bin/ls", &guest(&work.join("other"))],
    ];
    for denied in attempts {
        let output = run(&client, id, &denied);
        assert_ne!(
            output.outcome,
            ExecOutcome::Exited(0),
            "{denied:?}: {output:?}"
        );
    }
    let written = run(
        &client,
        id,
        &[
            "/bin/sh",
            "-c",
            "echo written > \"$1\"",
            "sh",
            &format!("{guest_out}/result"),
        ],
    );
    assert_eq!(written.outcome, ExecOutcome::Exited(0), "{written:?}");
    assert_eq!(
        std::fs::read_to_string(out.join("result")).unwrap(),
        "written\n"
    );
    assert_graceful(client.stop(id));
    client.deprovision(&sandbox.release()).unwrap();
    assert!(!src.join("new").exists() && !frozen.join("new").exists());
    assert_eq!(
        std::fs::read_to_string(frozen.join("b.txt")).unwrap(),
        "frozen"
    );
    assert_no_new_openvmm(&before);
}

/// Reports the workload's capabilities and the outcome of operations that would lift its mount
/// restrictions, given a read-only mapping inside a read-write one and a file path in the latter.
const CONTAINMENT_PROBE: &str = r#"
import ctypes, errno, json, sys
libc = ctypes.CDLL(None, use_errno=True)
frozen, writable = sys.argv[1], sys.argv[2]
def call(name, *args):
    ctypes.set_errno(0)
    if getattr(libc, name)(*args) == 0:
        return "ok"
    return errno.errorcode.get(ctypes.get_errno(), str(ctypes.get_errno()))
def write(path):
    try:
        with open(path, "w") as file:
            file.write("x")
        return "ok"
    except OSError as error:
        return errno.errorcode.get(error.errno, str(error.errno))
with open("/proc/self/status") as file:
    status = dict(line.split(":", 1) for line in file.read().splitlines() if ":" in line)
with open("/proc/sys/user/max_user_namespaces") as file:
    user_namespaces = file.read().strip()
names = ("CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb", "NoNewPrivs")
print(json.dumps({
    "capabilities": [status[name].strip() for name in names],
    "userNamespaces": user_namespaces,
    "write": write(writable),
    "writeFrozen": write(frozen + "/new"),
    "remount": call("mount", None, frozen.encode(), None, ctypes.c_ulong(32 | 4096), None),
    "tmpfs": call("mount", b"tmpfs", b"/tmp", b"tmpfs", ctypes.c_ulong(0), None),
    "export": call("mount", b"microvm", b"/mnt", b"virtiofs", ctypes.c_ulong(0), None),
    "unmount": call("umount2", frozen.encode(), 2),
    "unshareMount": call("unshare", 0x20000),
    "unshareUser": call("unshare", 0x10000000),
    "chroot": call("chroot", b"/tmp"),
}))
"#;

#[test]
#[ignore = "requires an approved private library, edge initramfs, GPT image, and a hypervisor host"]
fn workloads_cannot_lift_their_mount_restrictions() {
    let state = tempfile::tempdir().unwrap();
    let host = tempfile::tempdir().unwrap();
    let before = openvmm_processes();
    let out = host.path().join("out");
    let frozen = out.join("frozen");
    std::fs::create_dir_all(&frozen).unwrap();
    let request = ProvisionRequest::new().with_filesystem(FilesystemPolicy {
        readonly_paths: vec![frozen.clone()],
        readwrite_paths: vec![out.clone()],
        ..FilesystemPolicy::default()
    });
    let backend = backend(state.path());
    let client = AciEdgeSandbox::from_shared(backend.clone());
    let sandbox = started(&client, &backend, &request);
    let printed = python(
        &client,
        &sandbox.id,
        CONTAINMENT_PROBE,
        &[&guest(&frozen), &format!("{}/written", guest(&out))],
    );
    let report: serde_json::Value = serde_json::from_str(&printed).unwrap();

    // The workload stays root, with only the default container capabilities: CHOWN,
    // DAC_OVERRIDE, FOWNER, FSETID, KILL, SETGID, SETUID, NET_BIND_SERVICE, and AUDIT_WRITE.
    let (none, default) = ("0000000000000000", "00000000200004fb");
    assert_eq!(
        report["capabilities"],
        serde_json::json!([none, default, default, default, none, "1"]),
        "{report}"
    );
    assert_eq!(report["userNamespaces"], "0", "{report}");
    assert_eq!(report["write"], "ok", "{report}");
    assert_eq!(report["writeFrozen"], "EROFS", "{report}");
    for operation in [
        "remount",
        "tmpfs",
        "export",
        "unmount",
        "unshareMount",
        "chroot",
    ] {
        assert_eq!(report[operation], "EPERM", "{operation}: {report}");
    }
    assert_ne!(report["unshareUser"], "ok", "{report}");
    assert_graceful(client.stop(&sandbox.id));
    client.deprovision(&sandbox.release()).unwrap();
    assert!(out.join("written").is_file() && !frozen.join("new").exists());
    assert_no_new_openvmm(&before);
}

#[test]
#[ignore = "requires an approved private library, edge initramfs, GPT image, and a hypervisor host"]
fn replaced_host_objects_inside_a_writable_mapping_fail_the_next_start() {
    let state = tempfile::tempdir().unwrap();
    let host = tempfile::tempdir().unwrap();
    let before = openvmm_processes();
    let out = host.path().join("out");
    let (frozen, secret, moved) = (out.join("frozen"), out.join("secret"), out.join("moved"));
    for directory in [&frozen, &secret] {
        std::fs::create_dir_all(directory).unwrap();
    }
    let request = ProvisionRequest::new().with_filesystem(FilesystemPolicy {
        readonly_paths: vec![frozen.clone()],
        readwrite_paths: vec![out.clone()],
        denied_paths: vec![secret.clone()],
    });
    let backend = backend(state.path());
    let client = AciEdgeSandbox::from_shared(backend.clone());
    let sandbox = started(&client, &backend, &request);
    let id = &sandbox.id;
    assert_prints(&client, id, "planned");
    assert_graceful(client.stop(id));

    // A decoy at a protected path fails the start until the planned object is back.
    for pinned in [&secret, &frozen] {
        std::fs::rename(pinned, &moved).unwrap();
        std::fs::create_dir(pinned).unwrap();
        let error = client
            .start(id)
            .expect_err("a start must fail when a protected object was replaced");
        assert_eq!(error.code(), ErrorCode::BackendError, "{error}");
        assert!(error.message().contains("provision it again"), "{error}");
        std::fs::remove_dir(pinned).unwrap();
        std::fs::rename(&moved, pinned).unwrap();
        client.start(id).unwrap();
        assert_prints(&client, id, "restored");
        assert_graceful(client.stop(id));
    }
    client.deprovision(&sandbox.release()).unwrap();
    assert_no_new_openvmm(&before);
}

#[test]
#[ignore = "requires an approved private library, edge initramfs, GPT image, and a hypervisor host"]
fn every_mapping_that_fits_the_kernel_command_line_reaches_the_guest() {
    let state = tempfile::tempdir().unwrap();
    let host = tempfile::tempdir().unwrap();
    let before = openvmm_processes();
    let directories: Vec<PathBuf> = (0..64)
        .map(|index| {
            let directory = host.path().join(format!("m{index:02}"));
            std::fs::create_dir(&directory).unwrap();
            std::fs::write(directory.join("index"), format!("{index} ")).unwrap();
            directory
        })
        .collect();
    let request = |count: usize| {
        ProvisionRequest::new().with_filesystem(FilesystemPolicy {
            readonly_paths: directories[..count].to_vec(),
            ..FilesystemPolicy::default()
        })
    };
    let backend = backend(state.path());
    let client = AciEdgeSandbox::from_shared(backend.clone());

    // Provisioning plans the mappings without a guest, so it finds the largest set that fits.
    let mut fitting = 0;
    for count in 1..=directories.len() {
        match client.provision(&request(count)) {
            Ok(provisioned) => {
                client.deprovision(&provisioned.sandbox_id).unwrap();
                fitting = count;
            }
            Err(error) => {
                assert_eq!(error.code(), ErrorCode::PolicyValidation, "{error}");
                assert!(error.message().contains("kernel command line"), "{error}");
                break;
            }
        }
    }
    assert!(
        (2..directories.len()).contains(&fitting),
        "{fitting} mappings fit the kernel command line"
    );
    let sandbox = started(&client, &backend, &request(fitting));
    let files: Vec<String> = directories[..fitting]
        .iter()
        .map(|directory| format!("{}/index", guest(directory)))
        .collect();
    let mut argv = vec!["/bin/cat"];
    argv.extend(files.iter().map(String::as_str));
    let output = run(&client, &sandbox.id, &argv);
    let expected: String = (0..fitting).map(|index| format!("{index} ")).collect();
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        expected,
        "{output:?}"
    );
    eprintln!("{fitting} mappings fit the kernel command line");
    assert_graceful(client.stop(&sandbox.id));
    client.deprovision(&sandbox.release()).unwrap();
    assert_no_new_openvmm(&before);
}

/// Reports the guest's interfaces, source address, and resolver, and whether it reaches the
/// gateway's DNS service, two host services that greet it, and a host-loopback service through
/// the gateway.
const NETWORK_PROBE: &str = r#"
import json, socket, sys
gateway, host, allowed, other, loopback = sys.argv[1:6]
def connect(address, port, greeted=True):
    try:
        with socket.create_connection((address, int(port)), timeout=3) as connection:
            return "reached:" + connection.recv(16).decode() if greeted else "reached"
    except OSError:
        return "blocked"
def source():
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
            probe.connect((gateway, 53))
            return probe.getsockname()[0]
    except OSError:
        return "none"
def resolver():
    try:
        with open("/etc/resolv.conf") as file:
            return file.read()
    except OSError:
        return None
print(json.dumps({
    "interfaces": sorted(name for _, name in socket.if_nameindex()),
    "source": source(),
    "dns": connect(gateway, 53, greeted=False),
    "allowed": connect(host, allowed),
    "other": connect(host, other),
    "loopback": connect(gateway, loopback),
    "resolver": resolver(),
}))
"#;

/// Returns the host's IPv4 address on its default route.
fn host_address() -> Ipv4Addr {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
    // Connecting a UDP socket selects a route and a source address without sending anything.
    socket
        .connect((Ipv4Addr::new(192, 0, 2, 1), 9))
        .expect("the host needs a default IPv4 route");
    match socket.local_addr().unwrap().ip() {
        IpAddr::V4(address) if !address.is_loopback() && !address.is_unspecified() => address,
        address => panic!("the host's default route uses {address}"),
    }
}

/// Host TCP services that greet every connection with their name until they are dropped.
struct Services {
    done: Arc<AtomicBool>,
    threads: Vec<thread::JoinHandle<()>>,
}

impl Services {
    fn serve(listeners: Vec<(TcpListener, &'static str)>) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let threads = listeners
            .into_iter()
            .map(|(listener, greeting)| {
                listener.set_nonblocking(true).unwrap();
                let done = done.clone();
                thread::spawn(move || {
                    while !done.load(Ordering::Relaxed) {
                        match listener.accept() {
                            Ok((mut stream, _)) => {
                                let _ = stream.set_nonblocking(false);
                                let _ = stream.write_all(greeting.as_bytes());
                            }
                            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                                thread::sleep(Duration::from_millis(20));
                            }
                            Err(error) => panic!("the {greeting} service failed: {error}"),
                        }
                    }
                })
            })
            .collect();
        Self { done, threads }
    }
}

impl Drop for Services {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

#[test]
#[ignore = "requires an approved private library, edge initramfs, GPT image, and a hypervisor host"]
fn network_policies_are_enforced() {
    let state = tempfile::tempdir().unwrap();
    let before = openvmm_processes();
    let host = host_address();
    // The guest must reach the host through its gateway rather than consider it on-link.
    let (guest_network, guest_address, gateway) = if host.octets()[..3] == [10, 0, 0] {
        ("10.0.1.2/24", "10.0.1.2", "10.0.1.1")
    } else {
        ("10.0.0.2/24", "10.0.0.2", "10.0.0.1")
    };
    let listen = |address: Ipv4Addr| TcpListener::bind((address, 0)).unwrap();
    let (allowed, other, loopback) = (listen(host), listen(host), listen(Ipv4Addr::LOCALHOST));
    let ports = [&allowed, &other, &loopback]
        .map(|listener| listener.local_addr().unwrap().port().to_string());
    let _services = Services::serve(vec![
        (allowed, "allowed"),
        (other, "other"),
        (loopback, "loopback"),
    ]);
    let backend = backend_with(state.path(), false, |config| {
        config.guest_network = guest_network.to_owned();
    });
    let client = AciEdgeSandbox::from_shared(backend.clone());
    let host_rule = || NetworkRule::to(format!("{host}/32"));
    let contained = |egress: EgressPolicy| NetworkPolicy {
        egress,
        ..NetworkPolicy::deny_all()
    };
    // Each case lists whether the guest reaches the gateway's DNS service, the allowed host
    // service, and the other host service. No case reaches the host's loopback.
    let cases = [
        (
            "no device",
            NetworkPolicy::deny_all(),
            [false, false, false],
        ),
        (
            "allow",
            NetworkPolicy::egress(Access::Allow),
            [true, true, true],
        ),
        (
            "host allow rule",
            contained(
                EgressPolicy::new(Access::Deny)
                    .with_allow(host_rule().on_port(Protocol::Tcp, ports[0].parse().unwrap())),
            ),
            [false, true, false],
        ),
        (
            "host deny rule",
            contained(EgressPolicy::new(Access::Allow).with_deny(host_rule())),
            [true, false, false],
        ),
        (
            "gateway DNS rule",
            contained(
                EgressPolicy::new(Access::Deny)
                    .with_allow(NetworkRule::to(gateway).on_port(Protocol::Tcp, 53)),
            ),
            [true, false, false],
        ),
    ];
    let host_text = host.to_string();
    let reached = |yes: bool, greeting: &str| match (yes, greeting) {
        (false, _) => "blocked".to_owned(),
        (true, "") => "reached".to_owned(),
        (true, greeting) => format!("reached:{greeting}"),
    };
    let gateway_resolver = serde_json::json!(format!("nameserver {gateway}\n"));
    for (name, policy, [dns, allowed, other]) in cases {
        let nic = name != "no device";
        let sandbox = started(
            &client,
            &backend,
            &ProvisionRequest::new().with_network(policy),
        );
        let printed = python(
            &client,
            &sandbox.id,
            NETWORK_PROBE,
            &[gateway, &host_text, &ports[0], &ports[1], &ports[2]],
        );
        let mut report: serde_json::Value = serde_json::from_str(&printed).unwrap();
        let fields = report.as_object_mut().unwrap();
        let resolver = fields.remove("resolver");
        let interfaces = fields.remove("interfaces").unwrap();
        let (interface_count, source) = if nic { (2, guest_address) } else { (1, "none") };
        assert!(
            interfaces.as_array().unwrap().len() == interface_count
                && interfaces
                    .as_array()
                    .unwrap()
                    .contains(&serde_json::json!("lo")),
            "{name}: interfaces {interfaces}"
        );
        assert_eq!(
            report,
            serde_json::json!({
                "source": source,
                "dns": reached(dns, ""),
                "allowed": reached(allowed, "allowed"),
                "other": reached(other, "other"),
                "loopback": "blocked",
            }),
            "{name}"
        );
        // The guest names its gateway as resolver only when it may reach its DNS service.
        assert_eq!(
            resolver.as_ref() == Some(&gateway_resolver),
            dns,
            "{name}: resolver {resolver:?}"
        );
        assert_graceful(client.stop(&sandbox.id));
        client.deprovision(&sandbox.release()).unwrap();
    }
    assert_no_new_openvmm(&before);
}
