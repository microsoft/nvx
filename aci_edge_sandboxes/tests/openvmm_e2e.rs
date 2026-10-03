//! End-to-end lifecycle test against a real hypervisor and NVX guest artifacts.
//!
//! Run it through `scripts/nvx.py test-aci-edge-sandboxes --backend <kvm|mshv|whp>`, which resolves the
//! artifacts and sets these variables:
//!
//! - `ACI_EDGE_SANDBOXES_E2E_OPENVMM`, `ACI_EDGE_SANDBOXES_E2E_KERNEL`, `ACI_EDGE_SANDBOXES_E2E_INITRD`: OpenVMM and the Alpine guest.
//! - `ACI_EDGE_SANDBOXES_E2E_HYPERVISOR`: `kvm`, `mshv`, or `whp`.
//! - `ACI_EDGE_SANDBOXES_E2E_STATE_ROOT` (optional): sandbox state directory.
//! - `ACI_EDGE_SANDBOXES_E2E_OUTPUT_DIR` (optional): receives `openvmm-<token>.log`, the OpenVMM log
//!   of each sandbox whose test fails, named after the sandbox ID's token.
#![cfg(feature = "openvmm")]

use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use aci_edge_sandboxes::openvmm::{
    OpenVmmBackend, OpenVmmConfig, resolve_guest_path as guest_path,
};
use aci_edge_sandboxes::{
    Access, AciEdgeSandbox, EgressPolicy, ErrorCode, ExecOutcome, ExecOutput, ExecRequest,
    FilesystemPolicy, NetworkPolicy, NetworkRule, Protocol, ProvisionRequest, SandboxId, StdinMode,
};

mod support;

fn variable(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| {
        panic!("{name} must be set; run scripts/nvx.py test-aci-edge-sandboxes")
    })
}

/// Stops and deprovisions the sandbox and keeps the OpenVMM log when the test fails.
struct Cleanup {
    nvx: AciEdgeSandbox,
    backend: Arc<OpenVmmBackend>,
    sandbox_id: Option<SandboxId>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let Some(sandbox_id) = self.sandbox_id.take() else {
            return;
        };
        if thread::panicking()
            && let Ok(output_dir) = env::var("ACI_EDGE_SANDBOXES_E2E_OUTPUT_DIR")
        {
            // The tests share the directory, so each failed sandbox keeps its own log.
            let _ = fs::create_dir_all(&output_dir);
            let destination =
                PathBuf::from(output_dir).join(format!("openvmm-{}.log", sandbox_id.token()));
            if fs::copy(self.backend.log_path(&sandbox_id), &destination).is_ok() {
                eprintln!(
                    "saved the OpenVMM log of {sandbox_id} to {}",
                    destination.display()
                );
            }
        }
        let _ = self.nvx.stop(&sandbox_id);
        let _ = self.nvx.deprovision(&sandbox_id);
    }
}

fn run(nvx: &AciEdgeSandbox, sandbox_id: &SandboxId, request: ExecRequest) -> ExecOutput {
    nvx.exec(sandbox_id, &request)
        .unwrap()
        .wait_with_output()
        .unwrap()
}

fn shell(nvx: &AciEdgeSandbox, sandbox_id: &SandboxId, command_line: &str) -> ExecOutput {
    run(nvx, sandbox_id, ExecRequest::command_line(command_line))
}

/// Creates a client over the artifacts named by the environment.
fn client(name: &str) -> (AciEdgeSandbox, Arc<OpenVmmBackend>) {
    let state_root = env::var("ACI_EDGE_SANDBOXES_E2E_STATE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            env::temp_dir().join(format!("aci-edge-sandboxes-e2e-{}", std::process::id()))
        })
        .join(name);
    let config = OpenVmmConfig::new(
        variable("ACI_EDGE_SANDBOXES_E2E_OPENVMM"),
        variable("ACI_EDGE_SANDBOXES_E2E_KERNEL"),
        variable("ACI_EDGE_SANDBOXES_E2E_INITRD"),
        variable("ACI_EDGE_SANDBOXES_E2E_HYPERVISOR")
            .parse()
            .unwrap(),
        state_root,
    );
    let backend = Arc::new(OpenVmmBackend::new(config).unwrap());
    (AciEdgeSandbox::from_shared(backend.clone()), backend)
}

/// Provisions and starts a sandbox that is stopped and deprovisioned when dropped.
fn started(
    nvx: &AciEdgeSandbox,
    backend: &Arc<OpenVmmBackend>,
    request: &ProvisionRequest,
) -> Cleanup {
    let sandbox_id = nvx.provision(request).unwrap().sandbox_id;
    let cleanup = Cleanup {
        nvx: nvx.clone(),
        backend: backend.clone(),
        sandbox_id: Some(sandbox_id.clone()),
    };
    nvx.start(&sandbox_id).unwrap();
    cleanup
}

fn id(cleanup: &Cleanup) -> &SandboxId {
    cleanup.sandbox_id.as_ref().unwrap()
}

#[test]
#[ignore = "requires a hypervisor and NVX guest artifacts; run scripts/nvx.py test-aci-edge-sandboxes"]
fn host_paths_are_mapped_into_the_guest() {
    let host = tempfile::tempdir().unwrap();
    let work = host.path().join("work");
    for directory in ["src/secret", "out", "other"] {
        fs::create_dir_all(work.join(directory)).unwrap();
    }
    fs::write(work.join("src").join("a.txt"), "source").unwrap();
    fs::write(work.join("src").join("secret").join("key"), "hidden").unwrap();
    fs::write(work.join("config.json"), "{}").unwrap();
    let request = ProvisionRequest::new().with_filesystem(FilesystemPolicy {
        // The nested file mapping lies inside the read-only src mapping.
        readonly_paths: vec![
            work.join("src"),
            work.join("config.json"),
            work.join("src").join("a.txt"),
        ],
        readwrite_paths: vec![work.join("out")],
        denied_paths: vec![work.join("src").join("secret")],
    });
    let (nvx, backend) = client("filesystem");
    let sandbox = started(&nvx, &backend, &request);
    let sandbox_id = id(&sandbox);
    let guest = |path: PathBuf| guest_path(&path).unwrap();
    let (src, out, config) = (
        guest(work.join("src")),
        guest(work.join("out")),
        guest(work.join("config.json")),
    );
    // Paths travel as separate arguments rather than through a shell command line, so a host
    // temporary directory that contains spaces or quotes still addresses the intended files.
    let command = |argv: &[&str]| ExecRequest::argv(argv.iter().copied());

    assert_eq!(
        run(
            &nvx,
            sandbox_id,
            command(&["/bin/cat", &format!("{src}/a.txt"), &config])
        )
        .stdout,
        b"source{}"
    );
    let listing = run(&nvx, sandbox_id, command(&["/bin/ls", "-a", &src]));
    assert_eq!(listing.stdout, b".\n..\na.txt\n", "{listing:?}");
    for denied in [
        command(&["/bin/cat", &format!("{src}/secret/key")]),
        command(&["/bin/touch", &format!("{src}/new")]),
        command(&["/bin/touch", &config]),
        command(&["/bin/ls", "/run/nvx/hostfs"]),
        command(&["/bin/ls", &guest(work.join("other"))]),
    ] {
        let output = run(&nvx, sandbox_id, denied.clone());
        assert_ne!(
            output.outcome,
            ExecOutcome::Exited(0),
            "{denied:?}: {output:?}"
        );
    }
    assert!(
        run(
            &nvx,
            sandbox_id,
            command(&[
                "/bin/sh",
                "-c",
                "echo written > \"$1\"",
                "sh",
                &format!("{out}/result"),
            ])
        )
        .outcome
        .success()
    );
    assert_eq!(
        fs::read_to_string(work.join("out").join("result")).unwrap(),
        "written\n"
    );
    assert!(!work.join("src").join("new").exists());
    let pwd = run(
        &nvx,
        sandbox_id,
        ExecRequest::command_line("pwd; ls").with_cwd(out.clone()),
    );
    assert_eq!(pwd.stdout, format!("{out}\nresult\n").into_bytes());
    let missing = run(
        &nvx,
        sandbox_id,
        ExecRequest::argv(["/bin/true"]).with_cwd(format!("{out}/missing")),
    );
    assert_eq!(missing.outcome, ExecOutcome::Exited(125));
}

/// Returns the sorted `NAME=VALUE` lines that `/usr/bin/env` prints for `request`.
fn environment(nvx: &AciEdgeSandbox, sandbox_id: &SandboxId, request: ExecRequest) -> Vec<String> {
    let output = run(nvx, sandbox_id, request);
    assert!(output.outcome.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let mut lines: Vec<String> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    lines.sort();
    lines
}

#[test]
#[ignore = "requires a hypervisor and NVX guest artifacts; run scripts/nvx.py test-aci-edge-sandboxes"]
fn exec_environments_follow_the_mxc_schema() {
    let (nvx, backend) = client("environment");
    let sandbox = started(&nvx, &backend, &ProvisionRequest::new());
    let sandbox_id = id(&sandbox);
    // Programs run directly, because a shell may add variables of its own to its environment.
    let env = || ExecRequest::argv(["/usr/bin/env"]);
    let listing = |request| environment(&nvx, sandbox_id, request);

    // No environment supplied: the guest's default environment, which never holds host variables.
    let default = listing(env());
    for expected in [
        "PATH=/usr/sbin:/usr/bin:/sbin:/bin",
        "TERM=linux",
        "USER=nobody",
        "LOGNAME=nobody",
    ] {
        assert!(default.iter().any(|line| line == expected), "{default:?}");
    }
    assert!(default.iter().any(|line| line.starts_with("HOME=")));
    assert!(
        !default
            .iter()
            .any(|line| line.starts_with("ACI_EDGE_SANDBOXES_")),
        "{default:?}"
    );
    // `inheritDefaultEnv` selects nothing without entries.
    for inherit in [true, false] {
        assert_eq!(listing(env().with_inherit_default_env(inherit)), default);
    }

    // An explicitly empty environment starts the workload without any variable.
    for request in [
        env().with_environment(Vec::<String>::new()),
        env()
            .with_environment(Vec::<String>::new())
            .with_inherit_default_env(false),
    ] {
        let empty = run(&nvx, sandbox_id, request);
        assert_eq!(empty.outcome, ExecOutcome::Exited(0), "{empty:?}");
        assert!(
            empty.stdout.is_empty() && empty.stderr.is_empty(),
            "{empty:?}"
        );
    }
    // Layering nothing over the default environment is the default environment.
    assert_eq!(
        listing(
            env()
                .with_environment(Vec::<String>::new())
                .with_inherit_default_env(true)
        ),
        default
    );

    // Entries are the whole environment, and an empty value stays an empty value.
    assert_eq!(
        listing(env().with_environment(["FOO=bar", "EMPTY="])),
        ["EMPTY=", "FOO=bar"]
    );
    assert_eq!(
        listing(env().with_env("FOO=bar").with_inherit_default_env(false)),
        ["FOO=bar"]
    );
    // The last of repeated names wins.
    assert_eq!(
        listing(env().with_environment(["A=1", "B=2", "A=3"])),
        ["A=3", "B=2"]
    );
    // A shell receives the entries too, and keeps none of the default environment.
    let output = run(
        &nvx,
        sandbox_id,
        ExecRequest::command_line(
            "printf '%s|%s|%s' \"$FOO\" \"${EMPTY-unset}\" \"${HOME-unset}\"",
        )
        .with_environment(["FOO=bar", "EMPTY="]),
    );
    assert_eq!(output.stdout, b"bar||unset", "{output:?}");

    // Layered entries come on top of the default environment, and an entry replaces a default.
    let mut expected: Vec<String> = default
        .iter()
        .filter(|line| !line.starts_with("PATH="))
        .cloned()
        .chain(["FOO=bar".to_owned(), "PATH=/custom".to_owned()])
        .collect();
    expected.sort();
    assert_eq!(
        listing(
            env()
                .with_environment(["FOO=bar", "PATH=/custom"])
                .with_inherit_default_env(true)
        ),
        expected
    );

    // Each execution has its own environment: nothing carries over to the next one.
    let foo = |entry: Option<&str>| {
        let request = ExecRequest::argv(["/bin/printenv", "FOO"]);
        run(
            &nvx,
            sandbox_id,
            match entry {
                Some(entry) => request.with_env(entry),
                None => request,
            },
        )
    };
    for (entry, stdout) in [
        (Some("FOO=one"), "one\n"),
        (Some("FOO=two"), "two\n"),
        (Some("FOO="), "\n"),
        (Some("FOO=one"), "one\n"),
    ] {
        let output = foo(entry);
        assert_eq!(
            output.outcome,
            ExecOutcome::Exited(0),
            "{entry:?}: {output:?}"
        );
        assert_eq!(output.stdout, stdout.as_bytes(), "{entry:?}");
    }
    let unset = foo(None);
    assert_eq!(unset.outcome, ExecOutcome::Exited(1), "{unset:?}");
    assert!(unset.stdout.is_empty());
    assert_eq!(listing(env()), default);
}

#[test]
#[ignore = "requires a hypervisor and NVX guest artifacts; run scripts/nvx.py test-aci-edge-sandboxes"]
fn environment_values_reach_the_workload_exactly() {
    let (nvx, backend) = client("environment-values");
    let sandbox = started(&nvx, &backend, &ProvisionRequest::new());
    let sandbox_id = id(&sandbox);
    let values = [
        ("-DASHED", "1"),
        ("GREETING", "hello big world"),
        ("SPACED", "  leading and trailing  "),
        ("QUOTED", "\"double\" 'single' $HOME `date` ; | & > < \\"),
        ("MULTILINE", "first\nsecond"),
        ("UNICODE", "héllo ☃"),
        ("EQUALS", "a=b=c"),
        ("EMPTY", ""),
    ];
    let entries = || values.map(|(name, value)| format!("{name}={value}"));

    for (name, value) in values {
        let program = run(
            &nvx,
            sandbox_id,
            ExecRequest::argv(["/bin/printenv", "--", name]).with_environment(entries()),
        );
        assert_eq!(program.stdout, format!("{value}\n").as_bytes(), "{name}");
        // The workload need not be a program: a shell reads the same values.
        if !name.starts_with('-') {
            let shell = run(
                &nvx,
                sandbox_id,
                ExecRequest::command_line(format!("printf '%s' \"${name}\""))
                    .with_environment(entries()),
            );
            assert_eq!(shell.stdout, value.as_bytes(), "{name}");
        }
    }
    let all = run(
        &nvx,
        sandbox_id,
        ExecRequest::argv(["/usr/bin/env"]).with_environment(entries()),
    );
    let listing: String = values
        .iter()
        .map(|(name, value)| format!("{name}={value}\n"))
        .collect();
    assert_eq!(all.stdout, listing.as_bytes(), "{all:?}");

    // The program is read as a program even when its name contains `=`.
    assert!(
        shell(
            &nvx,
            sandbox_id,
            "printf '#!/bin/sh\\necho ran \"$FOO\"\\n' > '/tmp/a=b' && chmod +x '/tmp/a=b'",
        )
        .outcome
        .success()
    );
    for request in [
        ExecRequest::argv(["/tmp/a=b"]).with_env("FOO=bar baz"),
        ExecRequest::argv(["/tmp/a=b"])
            .with_env("FOO=bar baz")
            .with_inherit_default_env(true),
    ] {
        let output = run(&nvx, sandbox_id, request);
        assert_eq!(output.stdout, b"ran bar baz\n", "{output:?}");
        assert!(output.outcome.success(), "{output:?}");
    }

    // The guest agent takes 64 arguments of 4096 bytes: `env`, `-i`, and `--` use three of them.
    let entries: Vec<String> = (0..60).map(|index| format!("V{index}=x")).collect();
    let full = run(
        &nvx,
        sandbox_id,
        ExecRequest::argv(["/usr/bin/env"]).with_environment(entries.clone()),
    );
    let expected: String = entries.iter().map(|entry| format!("{entry}\n")).collect();
    assert_eq!(full.stdout, expected.as_bytes(), "{full:?}");
    let large = format!("BIG={}", "x".repeat(4096 - 4));
    let output = run(
        &nvx,
        sandbox_id,
        ExecRequest::argv(["/usr/bin/env"]).with_env(large.clone()),
    );
    assert_eq!(output.stdout, format!("{large}\n").as_bytes());
    let beyond = (0..61).map(|index| format!("V{index}=x"));
    let rejected = nvx
        .exec(
            sandbox_id,
            &ExecRequest::argv(["/usr/bin/env"]).with_environment(beyond),
        )
        .unwrap_err();
    assert_eq!(rejected.code(), ErrorCode::PolicyValidation);
}

#[test]
#[ignore = "requires a hypervisor and NVX guest artifacts; run scripts/nvx.py test-aci-edge-sandboxes"]
fn environments_leave_the_workload_contained() {
    let (nvx, backend) = client("environment-containment");
    let sandbox = started(&nvx, &backend, &ProvisionRequest::new());
    let sandbox_id = id(&sandbox);

    // The entries are applied after the workload lost its privileges, so it keeps its identity,
    // no capabilities, and `no_new_privs`.
    let probe =
        || ExecRequest::command_line("id -u; grep -E '^(CapEff|NoNewPrivs):' /proc/self/status");
    for request in [
        probe(),
        probe().with_env("FOO=bar"),
        probe().with_environment(Vec::<String>::new()),
        probe().with_env("FOO=bar").with_inherit_default_env(true),
    ] {
        let output = run(&nvx, sandbox_id, request.clone());
        assert_eq!(
            output.stdout, b"65534\nCapEff:\t0000000000000000\nNoNewPrivs:\t1\n",
            "{request:?}: {output:?}"
        );
    }

    // The working directory is entered with the environment in place.
    let output = run(
        &nvx,
        sandbox_id,
        ExecRequest::command_line("pwd; printf '%s' \"$FOO\"")
            .with_cwd("/tmp")
            .with_env("FOO=bar baz"),
    );
    assert_eq!(output.stdout, b"/tmp\nbar baz", "{output:?}");
    let missing = run(
        &nvx,
        sandbox_id,
        ExecRequest::argv(["/bin/true"])
            .with_cwd("/missing")
            .with_env("FOO=bar"),
    );
    assert_eq!(missing.outcome, ExecOutcome::Exited(125));

    // Timeouts and cancellation still reach the workload that the entries were applied to.
    let timed_out = run(
        &nvx,
        sandbox_id,
        ExecRequest::command_line("sleep 30")
            .with_env("FOO=bar")
            .with_timeout(Duration::from_millis(500)),
    );
    assert_eq!(timed_out.outcome, ExecOutcome::TimedOut);
    let started = Instant::now();
    let mut execution = nvx
        .exec(
            sandbox_id,
            &ExecRequest::command_line("echo started; sleep 30").with_environment(["FOO=bar"]),
        )
        .unwrap();
    let mut stdout = execution.take_stdout().unwrap();
    let mut first = [0u8; 8];
    std::io::Read::read_exact(&mut stdout, &mut first).unwrap();
    assert_eq!(&first, b"started\n");
    execution.canceller().cancel().unwrap();
    assert_eq!(execution.wait().unwrap(), ExecOutcome::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(10));
    let leftover = shell(&nvx, sandbox_id, "pgrep -x sleep || echo none");
    assert_eq!(leftover.stdout, b"none\n");
}

#[test]
#[ignore = "requires a hypervisor and NVX guest artifacts; run scripts/nvx.py test-aci-edge-sandboxes"]
fn network_policies_are_enforced() {
    // The guest reaches the portable profile's DNS service on its gateway over TCP. Probing it
    // needs no Internet access, so the test is deterministic.
    const PROBE: &str = "nc -w 3 10.0.0.1 53 </dev/null && echo reached || echo blocked";
    let (nvx, backend) = client("network");
    let cases: [(&str, NetworkPolicy, &[u8]); 5] = [
        ("no device", NetworkPolicy::deny_all(), b"lo\n"),
        ("allow", NetworkPolicy::egress(Access::Allow), b"reached\n"),
        (
            "allow rule",
            NetworkPolicy {
                egress: EgressPolicy::new(Access::Deny)
                    .with_allow(NetworkRule::to("10.0.0.1").on_port(Protocol::Tcp, 53)),
                ..NetworkPolicy::deny_all()
            },
            b"reached\n",
        ),
        (
            "other allow rule",
            NetworkPolicy {
                egress: EgressPolicy::new(Access::Deny)
                    .with_allow(NetworkRule::to("192.0.2.1").on_port(Protocol::Tcp, 53)),
                ..NetworkPolicy::deny_all()
            },
            b"blocked\n",
        ),
        (
            "deny rule",
            NetworkPolicy {
                egress: EgressPolicy::new(Access::Allow).with_deny(NetworkRule::to("10.0.0.0/24")),
                ..NetworkPolicy::deny_all()
            },
            b"blocked\n",
        ),
    ];
    for (name, policy, expected) in cases {
        let sandbox = started(
            &nvx,
            &backend,
            &ProvisionRequest::new().with_network(policy),
        );
        let command = if name == "no device" {
            "ls /sys/class/net"
        } else {
            PROBE
        };
        let output = shell(&nvx, id(&sandbox), command);
        assert_eq!(output.stdout, expected, "{name}: {output:?}");
    }
}

#[test]
#[ignore = "requires a hypervisor and NVX guest artifacts; run scripts/nvx.py test-aci-edge-sandboxes"]
fn openvmm_lifecycle_on_a_real_hypervisor() {
    let state_root = env::var("ACI_EDGE_SANDBOXES_E2E_STATE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            env::temp_dir().join(format!("aci-edge-sandboxes-e2e-{}", std::process::id()))
        });
    let config = OpenVmmConfig::new(
        variable("ACI_EDGE_SANDBOXES_E2E_OPENVMM"),
        variable("ACI_EDGE_SANDBOXES_E2E_KERNEL"),
        variable("ACI_EDGE_SANDBOXES_E2E_INITRD"),
        variable("ACI_EDGE_SANDBOXES_E2E_HYPERVISOR")
            .parse()
            .unwrap(),
        &state_root,
    );
    let backend = Arc::new(OpenVmmBackend::new(config).unwrap());
    let nvx = AciEdgeSandbox::from_shared(backend.clone());
    nvx.probe().unwrap();

    let sandbox_id = nvx.provision(&ProvisionRequest::new()).unwrap().sandbox_id;
    let mut cleanup = Cleanup {
        nvx: nvx.clone(),
        backend,
        sandbox_id: Some(sandbox_id.clone()),
    };
    assert_eq!(
        nvx.exec(&sandbox_id, &ExecRequest::command_line("true"))
            .unwrap_err()
            .code(),
        ErrorCode::NotStarted
    );

    let started = nvx.start(&sandbox_id).unwrap();
    println!("started {sandbox_id}: {:?}", started.metadata);
    assert_eq!(
        nvx.start(&sandbox_id).unwrap_err().code(),
        ErrorCode::AlreadyStarted
    );

    let hello = shell(&nvx, &sandbox_id, "echo hello");
    assert_eq!(hello.stdout, b"hello\n");
    assert_eq!(hello.outcome, ExecOutcome::Exited(0));
    let failure = shell(&nvx, &sandbox_id, "echo oops >&2; exit 3");
    assert_eq!(failure.stderr, b"oops\n");
    assert_eq!(failure.outcome, ExecOutcome::Exited(3));
    assert_eq!(shell(&nvx, &sandbox_id, "id -u").stdout, b"65534\n");
    // The workload runs directly in the guest's Alpine userland.
    let release = shell(&nvx, &sandbox_id, "cat /etc/alpine-release; hostname");
    assert!(release.outcome.success(), "{release:?}");
    let release = String::from_utf8(release.stdout).unwrap();
    assert!(release.starts_with("3."), "{release}");
    assert!(release.ends_with("\nnvx-sandbox\n"), "{release}");
    assert!(
        shell(&nvx, &sandbox_id, "test ! -e /run/nvx/rootfs")
            .outcome
            .success()
    );
    let argv = run(
        &nvx,
        &sandbox_id,
        ExecRequest::argv(["/bin/echo", "argv", "form"]),
    );
    assert_eq!(argv.stdout, b"argv form\n");
    let timed_out = run(
        &nvx,
        &sandbox_id,
        ExecRequest::command_line("sleep 30").with_timeout(Duration::from_millis(500)),
    );
    assert_eq!(timed_out.outcome, ExecOutcome::TimedOut);
    // Piped standard input is the one exec feature that remains unsupported.
    assert_eq!(
        nvx.exec(
            &sandbox_id,
            &ExecRequest::command_line("touch /tmp/rejected").with_stdin(StdinMode::Piped)
        )
        .unwrap_err()
        .code(),
        ErrorCode::PolicyValidation
    );

    // Cancellation kills every process of the workload and leaves the sandbox usable.
    let started = Instant::now();
    let mut execution = nvx
        .exec(
            &sandbox_id,
            &ExecRequest::command_line("setsid sleep 60 & echo started; sleep 30; echo late"),
        )
        .unwrap();
    let mut stdout = execution.take_stdout().unwrap();
    let mut first = [0u8; 8];
    std::io::Read::read_exact(&mut stdout, &mut first).unwrap();
    assert_eq!(&first, b"started\n");
    execution.canceller().cancel().unwrap();
    assert_eq!(execution.wait().unwrap(), ExecOutcome::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(10));
    let leftover = shell(&nvx, &sandbox_id, "pgrep -x sleep || echo none");
    assert_eq!(leftover.stdout, b"none\n");

    // No process of a workload outlives its exec, even one in its own session.
    let started = Instant::now();
    let detached = shell(
        &nvx,
        &sandbox_id,
        "setsid sleep 60 & sleep 60 & echo spawned",
    );
    assert_eq!(detached.stdout, b"spawned\n");
    assert!(detached.outcome.success(), "{detached:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
    let leftover = shell(&nvx, &sandbox_id, "pgrep -x sleep || echo none");
    assert_eq!(leftover.stdout, b"none\n");

    assert!(
        shell(
            &nvx,
            &sandbox_id,
            "printf kept > /tmp/aci_edge_sandboxes-e2e"
        )
        .outcome
        .success()
    );
    assert_eq!(
        shell(&nvx, &sandbox_id, "cat /tmp/aci_edge_sandboxes-e2e").stdout,
        b"kept"
    );
    let stopped = nvx.stop(&sandbox_id).unwrap();
    println!("stopped {sandbox_id}: {:?}", stopped.metadata);
    assert_eq!(stopped.metadata.unwrap()["forced"], false);
    assert_eq!(
        nvx.stop(&sandbox_id).unwrap_err().code(),
        ErrorCode::AlreadyStopped
    );

    // The root file system lives in guest memory, so a restart begins with fresh state.
    nvx.start(&sandbox_id).unwrap();
    assert!(
        shell(&nvx, &sandbox_id, "test ! -e /tmp/aci_edge_sandboxes-e2e")
            .outcome
            .success()
    );

    // The next start terminates a VM whose launching caller died before recording it.
    let orphan = support::interrupt_start(&state_root, &sandbox_id);
    nvx.start(&sandbox_id).unwrap();
    assert!(
        !support::process_running(orphan),
        "the VM of the interrupted start still runs"
    );
    assert_eq!(shell(&nvx, &sandbox_id, "echo again").stdout, b"again\n");
    nvx.stop(&sandbox_id).unwrap();

    nvx.deprovision(&sandbox_id).unwrap();
    cleanup.sandbox_id = None;
    assert_eq!(
        nvx.start(&sandbox_id).unwrap_err().code(),
        ErrorCode::StaleId
    );
}
