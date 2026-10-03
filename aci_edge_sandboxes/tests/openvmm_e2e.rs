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
    FilesystemPolicy, NetworkPolicy, NetworkRule, Protocol, ProvisionRequest, SandboxId,
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
    assert_eq!(
        nvx.exec(
            &sandbox_id,
            &ExecRequest::command_line("touch /tmp/rejected").with_env("MODE=test")
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
