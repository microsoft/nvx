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
    ExecRequest, FilesystemPolicy, NetworkPolicy, NetworkPort, NetworkRule, ProcessSpec, Protocol,
    ProvisionRequest, SandboxId, StdinMode,
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

    /// The host mapping table that the guest received, in order.
    fn mapping_table(&self, sandbox_id: &SandboxId) -> Vec<serde_json::Value> {
        let path = self
            .directory
            .path()
            .join(format!("fake-openvmm-{}-maps.json", sandbox_id.token()));
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

    fn lock_files(&self) -> usize {
        fs::read_dir(self.state_root.join(".locks"))
            .unwrap()
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
        environment(print().with_envs(Vec::<String>::new())),
        Vec::<String>::new()
    );
    assert_eq!(
        environment(
            print()
                .with_envs(Vec::<String>::new())
                .with_inherit_default_env(false)
        ),
        Vec::<String>::new()
    );

    // Supplied entries are the whole environment, empty values included.
    assert_eq!(
        environment(print().with_envs(["FOO=bar", "EMPTY="])),
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
    // The guest agent refuses a name that repeats, so the request fails before anything runs.
    let error = nvx
        .exec(&sandbox_id, &print().with_envs(["A=1", "B=2", "A=3"]))
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::PolicyValidation);
    assert!(error.message().contains("\"A\""), "{error}");

    // Layering keeps the default environment, and a supplied entry wins over a default.
    assert_eq!(
        environment(
            print()
                .with_envs(["FOO=bar", "PATH=/custom"])
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
                .with_envs(Vec::<String>::new())
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

    // A program lists the entries in the order in which they were supplied.
    for request in [
        ExecRequest::argv(["/usr/bin/env"]),
        ExecRequest::argv(["/usr/bin/env"]).with_cwd("/tmp"),
    ] {
        let output = output_of(&nvx, &sandbox_id, request.with_envs(environment()));
        assert_eq!(output.stdout, listing.as_bytes());
    }
    // A shell reads the same values.
    for (name, value) in entries {
        let output = output_of(
            &nvx,
            &sandbox_id,
            ExecRequest::command_line(format!("printenv {name}")).with_envs(environment()),
        );
        assert_eq!(output.stdout, format!("{value}\n").as_bytes(), "{name}");
    }
    // Printing the environment counts toward the output limit like any other output.
    let flooded = nvx
        .exec(
            &sandbox_id,
            &ExecRequest::command_line(format!("flood {}; env", 1024 * 1024 - 4))
                .with_env("FOO=bar"),
        )
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert_eq!(
        flooded.outcome,
        ExecOutcome::Failed(ExecFailure::OutputLimitExceeded)
    );
    assert_eq!(flooded.stdout.len(), 1024 * 1024 - 4);
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
    // `printenv` prints nothing and fails when the variable is unset.
    let assert_unset = |request: ExecRequest| {
        let output = nvx
            .exec(&sandbox_id, &request)
            .unwrap()
            .wait_with_output()
            .unwrap();
        assert_eq!(output.outcome, ExecOutcome::Exited(1), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
    };

    assert_eq!(foo(print().with_env("FOO=one")), b"one\n");
    assert_eq!(foo(print().with_env("FOO=two")), b"two\n");
    // Nothing is left over once a later execution supplies no environment.
    assert_unset(print());
    assert_unset(print().with_envs(Vec::<String>::new()));
    assert_eq!(foo(print().with_env("FOO=")), b"\n");
    assert_eq!(foo(print().with_env("FOO=one")), b"one\n");
    assert_unset(print());
    // A script exits with the status of its last command.
    let last_unset = nvx
        .exec(
            &sandbox_id,
            &ExecRequest::command_line("printenv BAR; printenv FOO").with_env("BAR=x"),
        )
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert_eq!(last_unset.outcome, ExecOutcome::Exited(1), "{last_unset:?}");
    assert_eq!(last_unset.stdout, b"x\n");
    assert_eq!(
        foo(ExecRequest::command_line("printenv FOO; printenv BAR").with_env("BAR=x")),
        b"x\n"
    );
    assert_unset(ExecRequest::argv(["/usr/bin/printenv", "FOO"]));
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
fn programs_get_exactly_the_requested_environment_in_any_working_directory() {
    let fixture = Fixture::new();
    let nvx = fixture.nvx();
    let sandbox_id = nvx.provision(&ProvisionRequest::new()).unwrap().sandbox_id;
    nvx.start(&sandbox_id).unwrap();
    let environment = |request| environment_of(&nvx, &sandbox_id, request);
    // A shell would export `PWD` and `SHLVL` (and `OLDPWD` once it changes directories) and
    // rewrite entries with these names, but no shell runs in front of a program.
    let shell_names = [
        "PWD=/custom",
        "SHLVL=7",
        "OLDPWD=/keep",
        "-DASHED=1",
        "FOO=bar",
    ];
    for cwd in [None, Some("/work")] {
        let print = || {
            let request = ExecRequest::argv(["/usr/bin/env"]);
            match cwd {
                Some(cwd) => request.with_cwd(cwd),
                None => request,
            }
        };
        assert_eq!(
            environment(print().with_env("FOO=bar")),
            sorted(["FOO=bar"])
        );
        assert_eq!(
            environment(print().with_envs(Vec::<String>::new())),
            Vec::<String>::new()
        );
        assert_eq!(
            environment(print().with_envs(shell_names)),
            sorted(shell_names)
        );
        // Layering puts the entries on top of the default environment, with nothing else added.
        assert_eq!(
            environment(
                print()
                    .with_envs(["PWD=/custom", "SHLVL=7", "FOO=bar"])
                    .with_inherit_default_env(true)
            ),
            sorted(
                DEFAULT_ENVIRONMENT
                    .into_iter()
                    .chain(["PWD=/custom", "SHLVL=7", "FOO=bar"])
            )
        );
    }
    let output = output_of(
        &nvx,
        &sandbox_id,
        ExecRequest::command_line("pwd; printenv FOO")
            .with_cwd("/work")
            .with_env("FOO=bar baz"),
    );
    assert_eq!(output.stdout, b"/work\nbar baz\n");
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
    // ...which holds the working directory...
    assert_eq!(
        environment(print().with_cwd("/work").with_env("FOO=bar")),
        sorted(["FOO=bar", "SHLVL=1", "PWD=/work"])
    );
    // ...and it rewrites entries with those names, as it does for any script.
    assert_eq!(
        environment(print().with_envs(["SHLVL=7", "PWD=/custom"])),
        sorted(["SHLVL=8", "PWD=/"])
    );
    // It reads `SHLVL` with `atoi` and exports one more as an unsigned integer, which wraps
    // around at the maximum.
    for (received, exported) in [
        ("SHLVL=4294967295", "SHLVL=0"),
        ("SHLVL=-3", "SHLVL=4294967294"),
        ("SHLVL=7x", "SHLVL=8"),
        ("SHLVL=abc", "SHLVL=1"),
    ] {
        assert_eq!(
            environment(print().with_env(received)),
            sorted([exported, "PWD=/"])
        );
    }
    assert_eq!(
        environment(print()),
        sorted(DEFAULT_ENVIRONMENT.into_iter().chain(["SHLVL=1", "PWD=/"]))
    );
    nvx.stop(&sandbox_id).unwrap();
    nvx.deprovision(&sandbox_id).unwrap();
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
fn calls_with_stale_ids_leave_no_lock_files() {
    let fixture = Fixture::new();
    let nvx = fixture.nvx();
    let deprovisioned = nvx.provision(&ProvisionRequest::new()).unwrap().sandbox_id;
    nvx.start(&deprovisioned).unwrap();
    nvx.stop(&deprovisioned).unwrap();
    assert_eq!(fixture.lock_files(), 1);
    nvx.deprovision(&deprovisioned).unwrap();
    assert_eq!(fixture.lock_files(), 0);

    let never_provisioned = SandboxId::generate().unwrap();
    for sandbox_id in [&deprovisioned, &never_provisioned] {
        for error in [
            nvx.start(sandbox_id).unwrap_err(),
            nvx.exec(sandbox_id, &ExecRequest::command_line("echo"))
                .unwrap_err(),
            nvx.stop(sandbox_id).unwrap_err(),
            nvx.deprovision(sandbox_id).unwrap_err(),
        ] {
            assert_eq!(error.code(), ErrorCode::StaleId, "{error}");
        }
    }
    assert_eq!(fixture.lock_files(), 0);
    assert_eq!(fixture.sandbox_dirs(), 0);
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
fn timeouts_cover_the_mxc_range() {
    let fixture = Fixture::new();
    let nvx = fixture.nvx();
    assert_eq!(
        nvx.capabilities().exec.max_timeout_ms,
        Some(ProcessSpec::MAX_TIMEOUT_MS)
    );
    let sandbox_id = nvx.provision(&ProvisionRequest::new()).unwrap().sandbox_id;
    nvx.start(&sandbox_id).unwrap();

    // Every timeout up to MXC's maximum reaches the guest agent unchanged, and a workload that
    // ends first is not held up by it.
    let started = Instant::now();
    for millis in [0, 3_600_001, 86_400_000, ProcessSpec::MAX_TIMEOUT_MS] {
        let output = output_of(
            &nvx,
            &sandbox_id,
            ExecRequest::command_line("timeout; sleep 50")
                .with_timeout(Duration::from_millis(millis)),
        );
        assert_eq!(output.stdout, format!("{millis}\n").as_bytes(), "{millis}");
    }
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    // Without a timeout, the guest agent receives zero, which disables it.
    let unbounded = output_of(&nvx, &sandbox_id, ExecRequest::command_line("timeout"));
    assert_eq!(unbounded.stdout, b"0\n");

    // A timeout still ends a workload that overruns it.
    let timed_out = nvx
        .exec(
            &sandbox_id,
            &ExecRequest::command_line("echo started; sleep 30000")
                .with_timeout(Duration::from_millis(200)),
        )
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert_eq!(timed_out.outcome, ExecOutcome::TimedOut);
    assert_eq!(timed_out.stdout, b"started\n");
    assert_eq!(run(&nvx, &sandbox_id, "echo after").stdout, b"after\n");

    nvx.stop(&sandbox_id).unwrap();
    nvx.deprovision(&sandbox_id).unwrap();
}

#[test]
fn working_directories_apply_to_each_execution() {
    let fixture = Fixture::new();
    let nvx = fixture.nvx();
    let sandbox_id = nvx.provision(&ProvisionRequest::new()).unwrap().sandbox_id;
    nvx.start(&sandbox_id).unwrap();
    let pwd = |cwd: Option<&str>| {
        let request = ExecRequest::argv(["/bin/pwd"]);
        let request = match cwd {
            Some(cwd) => request.with_cwd(cwd),
            None => request,
        };
        nvx.exec(&sandbox_id, &request)
            .unwrap()
            .wait_with_output()
            .unwrap()
    };

    // Without a working directory, a workload starts in the guest's root directory.
    assert_eq!(pwd(None).stdout, b"/\n");
    assert!(
        run(&nvx, &sandbox_id, "mkdir /work/a; mkdir /work/b")
            .outcome
            .success()
    );
    // Each execution starts in its own directory, and none carries over to the next.
    for (cwd, expected) in [
        (Some("/work/a"), "/work/a\n"),
        (Some("/work/b"), "/work/b\n"),
        (None, "/\n"),
        (Some("/work/a"), "/work/a\n"),
    ] {
        let output = pwd(cwd);
        assert_eq!(
            output.outcome,
            ExecOutcome::Exited(0),
            "{cwd:?}: {output:?}"
        );
        assert_eq!(output.stdout, expected.as_bytes(), "{cwd:?}");
    }
    let shell = nvx
        .exec(
            &sandbox_id,
            &ExecRequest::command_line("pwd").with_cwd("/work/b"),
        )
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert_eq!(shell.stdout, b"/work/b\n");

    // A missing directory fails the launch with a diagnostic instead of running elsewhere.
    let missing = nvx
        .exec(
            &sandbox_id,
            &ExecRequest::command_line("write ran yes").with_cwd("/work/missing"),
        )
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert_eq!(
        missing.outcome,
        ExecOutcome::Failed(ExecFailure::WorkingDirectory)
    );
    assert!(
        missing.outcome.to_string().contains("working directory"),
        "{}",
        missing.outcome
    );
    assert!(missing.stdout.is_empty());
    let diagnostic = String::from_utf8(missing.stderr).unwrap();
    assert!(
        diagnostic.contains("/work/missing") && diagnostic.contains("No such file or directory"),
        "{diagnostic}"
    );
    assert!(run(&nvx, &sandbox_id, "read ran").stdout.is_empty());
    assert_eq!(pwd(Some("/work/b")).stdout, b"/work/b\n");

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
    // Ports that are not adjacent each need their own OpenVMM rule.
    let mut ports = NetworkRule::to("192.0.2.0/24");
    ports.ports = (0..257)
        .map(|index| NetworkPort {
            protocol: Protocol::Tcp,
            port: Some(2 * index + 1),
            end_port: None,
        })
        .collect();
    let oversized = NetworkPolicy {
        egress: EgressPolicy::new(Access::Deny).with_allow(ports),
        ..NetworkPolicy::deny_all()
    };
    assert_eq!(
        nvx.provision(&ProvisionRequest::new().with_network(oversized))
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
        write().with_envs(["KEY=one", "KEY=two"]),
        write().with_cwd(format!("/{}", "d".repeat(4095))),
        write().with_stdin(StdinMode::Piped),
        write().with_env(format!("BIG={}", "x".repeat(5000))),
        write().with_envs((0..257).map(|index| format!("V{index}=x"))),
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
        write().with_envs(["A=1", "B"]),
        write().with_timeout(Duration::from_millis(ProcessSpec::MAX_TIMEOUT_MS + 1)),
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
        "workload accounts",
        "workload containment",
        "per-execution environments",
        "working directories",
        "host path mapping tables",
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

    // This guest agent predates working directories, so it would start workloads elsewhere. It
    // provides every other required feature.
    let without_cwd = fixture.nvx_with(|config| {
        config.kernel_command_line = "fake_guest_features=95".to_owned();
    });
    let error = without_cwd.start(&sandbox_id).unwrap_err();
    assert_eq!(error.code(), ErrorCode::BackendUnavailable, "{error}");
    assert!(
        error
            .message()
            .contains("the openvmm backend needs (working directories)"),
        "{error}"
    );

    // The previous release's guest agent read its bind mounts from the kernel command line, so
    // it would ignore the mapping table.
    let previous = fixture.nvx_with(|config| {
        config.kernel_command_line = "fake_guest_features=63".to_owned();
    });
    let error = previous.start(&sandbox_id).unwrap_err();
    assert_eq!(error.code(), ErrorCode::BackendUnavailable, "{error}");
    assert!(
        error
            .message()
            .contains("the openvmm backend needs (host path mapping tables)"),
        "{error}"
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
                .with_allow(NetworkRule::to("192.0.2.0/24").on_port(Protocol::Tcp, 443))
                .with_allow(NetworkRule::to("192.0.2.0/24").on_port_range(
                    Protocol::Tcp,
                    8000,
                    8010,
                ))
                .with_allow(NetworkRule::to("192.0.2.9").on_protocol(Protocol::Icmp))
                .with_allow(NetworkRule::to("2001:db8::/32").on_port(Protocol::Tcp, 443))
                .with_deny(NetworkRule::to("192.0.2.0/24").on_protocol(Protocol::Udp))
                .with_deny(NetworkRule::to("192.0.2.7").on_port(Protocol::Tcp, 8005))
                .with_deny(NetworkRule::to("2001:db8::1")),
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
    let mounts = value("--mount-child");
    assert_eq!(mounts.len(), 2, "{mounts:?}");
    assert!(
        mounts[0].starts_with("0,") && mounts[0].ends_with(",rw"),
        "{mounts:?}"
    );
    assert!(
        mounts[1].starts_with("1,") && mounts[1].ends_with(",ro"),
        "{mounts:?}"
    );
    assert_eq!(value("--mount-aggregate"), ["/run/nvx/hostfs/root"]);
    assert!(value("--mount").is_empty());
    assert_eq!(value("--mount-deny").len(), 1);
    assert_eq!(value("--network-egress"), ["deny"]);
    assert_eq!(
        value("--network-egress-allow"),
        [
            "192.0.2.0/24:tcp:443",
            "192.0.2.0/24:tcp:8000-8010",
            "192.0.2.9/32:icmp",
            "2001:db8::/32:tcp:443"
        ]
    );
    assert_eq!(
        value("--network-egress-deny"),
        [
            "192.0.2.0/24:udp",
            "192.0.2.7/32:tcp:8005",
            "2001:db8::1/128"
        ]
    );
    // Only the number of mappings travels on the kernel command line.
    let command_line = &value("--cmdline")[0];
    assert!(
        command_line.split(' ').any(|token| token == "nvx_maps=2"),
        "{command_line}"
    );
    assert!(!command_line.contains("nvx_map="), "{command_line}");
    let table = fixture.mapping_table(&sandbox_id);
    let entries: Vec<(&str, bool)> = table
        .iter()
        .map(|entry| {
            (
                entry["source"].as_str().unwrap(),
                entry["read_only"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(entries, [("0", false), ("1", true)]);

    let guest =
        aci_edge_sandboxes::openvmm::resolve_guest_path(&base.join("work").join("out")).unwrap();
    assert!(
        state_json(&fixture, &sandbox_id, "sandbox.json")["filesystem"]["binds"]
            .as_array()
            .unwrap()
            .iter()
            .any(|bind| bind["child"] == 0 && bind["source"] == "" && bind["target"] == guest),
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

#[test]
fn mapped_files_are_not_working_directories() {
    let fixture = Fixture::new();
    let nvx = fixture.nvx();
    let work = fixture.directory.path().join("work");
    fs::create_dir_all(work.join("out")).unwrap();
    fs::write(work.join("notes.txt"), b"notes").unwrap();
    let request = ProvisionRequest::new().with_filesystem(FilesystemPolicy {
        readonly_paths: vec![work.join("notes.txt")],
        readwrite_paths: vec![work.join("out")],
        denied_paths: Vec::new(),
    });
    let sandbox_id = nvx.provision(&request).unwrap().sandbox_id;
    nvx.start(&sandbox_id).unwrap();
    let guest = |name: &str| aci_edge_sandboxes::openvmm::resolve_guest_path(&work.join(name));

    let out = guest("out").unwrap();
    let directory = nvx
        .exec(
            &sandbox_id,
            &ExecRequest::command_line("pwd").with_cwd(out.clone()),
        )
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert_eq!(directory.stdout, format!("{out}\n").into_bytes());

    // As in the guest, a mapped regular file is no working directory, and nothing runs.
    let notes = guest("notes.txt").unwrap();
    let file = nvx
        .exec(
            &sandbox_id,
            &ExecRequest::command_line("write ran yes").with_cwd(notes.clone()),
        )
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert_eq!(
        file.outcome,
        ExecOutcome::Failed(ExecFailure::WorkingDirectory)
    );
    let diagnostic = String::from_utf8(file.stderr).unwrap();
    assert!(
        diagnostic.contains(&format!("working directory {notes}: Not a directory")),
        "{diagnostic}"
    );
    assert!(run(&nvx, &sandbox_id, "read ran").stdout.is_empty());

    nvx.stop(&sandbox_id).unwrap();
    nvx.deprovision(&sandbox_id).unwrap();
}

#[test]
fn unrelated_directories_are_exported_side_by_side() {
    let fixture = Fixture::new();
    let nvx = fixture.nvx();
    let base = fixture.directory.path();
    for name in ["projects/app", "projects/private", "build-output", "tools"] {
        fs::create_dir_all(base.join(name)).unwrap();
    }
    fs::write(base.join("tools").join("lint.cfg"), b"rules").unwrap();
    // The layout of microsoft/nvx#281: the paths share no directory that could be exported
    // alone without the unmapped projects/private.
    let request = ProvisionRequest::new().with_filesystem(FilesystemPolicy {
        readonly_paths: vec![base.join("tools")],
        readwrite_paths: vec![base.join("projects/app"), base.join("build-output")],
        denied_paths: Vec::new(),
    });
    let sandbox_id = nvx.provision(&request).unwrap().sandbox_id;
    nvx.start(&sandbox_id).unwrap();
    let arguments = fixture.launch_arguments(&sandbox_id);
    let children: Vec<&String> = arguments
        .windows(2)
        .filter(|pair| pair[0] == "--mount-child")
        .map(|pair| &pair[1])
        .collect();
    assert_eq!(children.len(), 3, "{arguments:?}");
    let modes: Vec<&str> = children
        .iter()
        .map(|child| child.rsplit(',').next().unwrap())
        .collect();
    assert_eq!(modes, ["rw", "rw", "ro"]);
    // Each mapped directory is a child of its own; their parent stays unexported.
    for (child, name) in children
        .iter()
        .zip(["build-output", "projects/app", "tools"])
    {
        let (_, rest) = child.split_once(',').unwrap();
        let (host, _) = rest.rsplit_once(',').unwrap();
        assert!(Path::new(host).ends_with(name), "{child}");
    }
    for name in ["projects/app", "build-output", "tools"] {
        let guest = aci_edge_sandboxes::openvmm::resolve_guest_path(&base.join(name)).unwrap();
        let pwd = nvx
            .exec(
                &sandbox_id,
                &ExecRequest::command_line("pwd").with_cwd(guest.clone()),
            )
            .unwrap()
            .wait_with_output()
            .unwrap();
        assert_eq!(pwd.stdout, format!("{guest}\n").into_bytes(), "{name}");
    }
    nvx.stop(&sandbox_id).unwrap();
    nvx.deprovision(&sandbox_id).unwrap();
}

#[test]
fn large_mapping_tables_reach_the_guest_in_several_records() {
    let fixture = Fixture::new();
    let nvx = fixture.nvx();
    let work = fixture.directory.path().join("work");
    // Read-only directories inside a read-write one need no OpenVMM options of their own, so
    // the table outgrows one 64 KiB control record long before the command line fills up.
    let names: Vec<String> = (0..800)
        .map(|index| format!("{}-{index:04}", "directory".repeat(6)))
        .collect();
    for name in &names {
        fs::create_dir_all(work.join(name)).unwrap();
    }
    let request = ProvisionRequest::new().with_filesystem(FilesystemPolicy {
        readonly_paths: names.iter().map(|name| work.join(name)).collect(),
        readwrite_paths: vec![work.clone()],
        denied_paths: Vec::new(),
    });
    let sandbox_id = nvx.provision(&request).unwrap().sandbox_id;
    nvx.start(&sandbox_id).unwrap();
    let command_line = fixture
        .launch_arguments(&sandbox_id)
        .windows(2)
        .find(|pair| pair[0] == "--cmdline")
        .map(|pair| pair[1].clone())
        .unwrap();
    assert!(command_line.contains("nvx_maps=801"), "{command_line}");
    let table = fixture.mapping_table(&sandbox_id);
    assert_eq!(table.len(), 801);
    let bytes: usize = table
        .iter()
        .map(|entry| {
            6 + entry["source"].as_str().unwrap().len() + entry["target"].as_str().unwrap().len()
        })
        .sum();
    assert!(bytes > 64 * 1024, "{bytes}");
    assert_eq!(table[0]["source"], "0");
    assert!(
        table[1..]
            .iter()
            .all(|entry| entry["read_only"] == true && entry["directory"] == true)
    );
    let last = aci_edge_sandboxes::openvmm::resolve_guest_path(&work.join(&names[799])).unwrap();
    let pwd = nvx
        .exec(
            &sandbox_id,
            &ExecRequest::command_line("pwd").with_cwd(last.clone()),
        )
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert_eq!(pwd.stdout, format!("{last}\n").into_bytes());
    nvx.stop(&sandbox_id).unwrap();
    nvx.deprovision(&sandbox_id).unwrap();
}

#[test]
fn starts_fail_when_the_guest_cannot_mount_the_mappings() {
    let fixture = Fixture::new();
    let work = fixture.directory.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let nvx = fixture.nvx_with(|config| {
        config.kernel_command_line = "fake_refuse_maps=1".to_owned();
    });
    let request = ProvisionRequest::new().with_filesystem(FilesystemPolicy {
        readwrite_paths: vec![work],
        ..FilesystemPolicy::default()
    });
    let sandbox_id = nvx.provision(&request).unwrap().sandbox_id;
    let error = nvx.start(&sandbox_id).unwrap_err();
    assert_eq!(error.code(), ErrorCode::BackendError, "{error}");
    assert!(
        error
            .message()
            .contains("could not mount a mapped host path (error 13)"),
        "{error}"
    );
    // The VM was terminated and forgotten, so the sandbox is merely provisioned.
    assert!(
        !fixture
            .state_root
            .join(sandbox_id.token())
            .join("runtime.json")
            .exists()
    );
    assert_eq!(
        nvx.exec(&sandbox_id, &ExecRequest::command_line("echo"))
            .unwrap_err()
            .code(),
        ErrorCode::NotStarted
    );
    nvx.deprovision(&sandbox_id).unwrap();
}

#[test]
fn sandboxes_of_the_previous_state_format_are_refused() {
    let fixture = Fixture::new();
    let nvx = fixture.nvx();
    let work = fixture.directory.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let request = ProvisionRequest::new().with_filesystem(FilesystemPolicy {
        readwrite_paths: vec![work.clone()],
        ..FilesystemPolicy::default()
    });
    let sandbox_id = nvx.provision(&request).unwrap().sandbox_id;
    // Version 2 exported one directory and listed its bind mounts on the kernel command line.
    let mut record = state_json(&fixture, &sandbox_id, "sandbox.json");
    record["format"] = 2.into();
    record["filesystem"] = serde_json::json!({
        "root": work,
        "writable": true,
        "denied": [],
        "deniedIdentities": [],
        "binds": [{ "source": "", "target": "/work", "readOnly": false }],
    });
    fs::write(
        fixture
            .state_root
            .join(sandbox_id.token())
            .join("sandbox.json"),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
    let error = nvx.start(&sandbox_id).unwrap_err();
    assert_eq!(error.code(), ErrorCode::BackendError, "{error}");
    assert!(error.message().contains("unsupported format"), "{error}");
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

#[cfg(feature = "async")]
#[test]
fn dropped_async_execs_do_not_hold_the_sandbox() {
    let fixture = Fixture::new();
    let nvx = fixture.nvx_with(|config| {
        config.control_timeout = Duration::from_secs(5);
        config.stop_timeout = Duration::from_secs(5);
    });
    let client = aci_edge_sandboxes::AsyncAciEdgeSandbox::new(nvx.clone());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let sandbox_id = nvx.provision(&ProvisionRequest::new()).unwrap().sandbox_id;
    nvx.start(&sandbox_id).unwrap();

    // The first execution holds the guest's only control slot, so the next one has to wait.
    let first = nvx
        .exec(
            &sandbox_id,
            &ExecRequest::command_line("sleep 1500; echo first"),
        )
        .unwrap();
    let abandoned = runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_millis(200),
            client.exec(
                sandbox_id.clone(),
                ExecRequest::command_line("write abandoned started; sleep 60000"),
            ),
        )
        .await
    });
    assert!(
        abandoned.is_err(),
        "the exec must wait for the control slot"
    );
    assert_eq!(first.wait_with_output().unwrap().stdout, b"first\n");

    // The abandoned workload starts once the slot is free. It must be cancelled at once, or it
    // would keep every later execution waiting for the slot until control_timeout fails it.
    let deadline = Instant::now() + Duration::from_secs(10);
    while run(&nvx, &sandbox_id, "read abandoned").stdout != b"started" {
        assert!(
            Instant::now() < deadline,
            "the abandoned workload never started"
        );
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(run(&nvx, &sandbox_id, "echo later").stdout, b"later\n");
    let stopped = nvx.stop(&sandbox_id).unwrap().metadata.unwrap();
    assert_eq!(stopped["forced"], false);
    nvx.deprovision(&sandbox_id).unwrap();
}
