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
    Access, AciEdgeSandbox, EgressPolicy, ErrorCode, ExecFailure, ExecOutcome, ExecOutput,
    ExecRequest, FilesystemPolicy, NetworkPeer, NetworkPolicy, NetworkRule, ProcessSpec, Protocol,
    ProvisionRequest, SandboxId, StdinMode,
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
///
/// Keep `name` short: on Unix, each sandbox's control socket path,
/// `<state root>/<name>/<32-character token>/control.sock`, must fit in 107 bytes, and the
/// state root of `scripts/nvx.py test-aci-edge-sandboxes` takes 42 of them under `/tmp`.
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
    assert_eq!(
        missing.outcome,
        ExecOutcome::Failed(ExecFailure::WorkingDirectory),
        "{missing:?}"
    );
}

/// A directory on another volume than the temporary directory, if the host has one: the one
/// that `ACI_EDGE_SANDBOXES_E2E_SECOND_VOLUME` names, or the runner's temporary directory, which
/// lies on another volume on GitHub's Windows runners. Tests create their files below it.
fn second_volume() -> Option<PathBuf> {
    let candidate = env::var_os("ACI_EDGE_SANDBOXES_E2E_SECOND_VOLUME")
        .or_else(|| env::var_os("RUNNER_TEMP"))
        .map(PathBuf::from)?;
    let candidate = fs::canonicalize(candidate).ok()?;
    let temporary = fs::canonicalize(env::temp_dir()).ok()?;
    #[cfg(unix)]
    let other = {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(&candidate).ok()?.dev() != fs::metadata(&temporary).ok()?.dev()
    };
    #[cfg(not(unix))]
    let other = candidate.components().next() != temporary.components().next();
    (other && candidate.is_dir()).then_some(candidate)
}

/// Runs `script` with `paths` as its positional parameters.
fn script(script: &str, paths: &[String]) -> ExecRequest {
    let mut argv = vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()];
    argv.push("sh".to_owned());
    argv.extend(paths.iter().cloned());
    ExecRequest::argv(argv)
}

#[test]
#[ignore = "requires a hypervisor and NVX guest artifacts; run scripts/nvx.py test-aci-edge-sandboxes"]
fn unrelated_directories_keep_their_own_access() {
    // The layout of microsoft/nvx#281: projects/app and build-output are read-write, tools is
    // read-only, and nothing else of the host is exported.
    let host = tempfile::tempdir().unwrap();
    let base = host.path();
    for directory in ["projects/app", "projects/private", "build-output", "tools"] {
        fs::create_dir_all(base.join(directory)).unwrap();
    }
    fs::write(base.join("tools").join("lint.cfg"), "rules").unwrap();
    fs::write(base.join("projects").join("private").join("key"), "secret").unwrap();
    let request = ProvisionRequest::new().with_filesystem(FilesystemPolicy {
        readonly_paths: vec![base.join("tools")],
        readwrite_paths: vec![base.join("projects/app"), base.join("build-output")],
        denied_paths: Vec::new(),
    });
    let (nvx, backend) = client("volumes");
    let sandbox = started(&nvx, &backend, &request);
    let sandbox_id = id(&sandbox);
    let guest = |name: &str| guest_path(&base.join(name)).unwrap();
    let paths = [
        guest("projects/app"),
        guest("build-output"),
        guest("tools"),
        guest("projects"),
    ];
    let output = run(
        &nvx,
        sandbox_id,
        script(
            "echo app > \"$1/result\" && echo build > \"$2/result\" && cat \"$3/lint.cfg\" \
             && ! touch \"$3/new\" 2>/dev/null && ls \"$4\"",
            &paths,
        ),
    );
    assert!(output.outcome.success(), "{output:?}");
    // The parent of projects/app holds only its mount point.
    assert_eq!(output.stdout, b"rulesapp\n", "{output:?}");
    assert_eq!(
        fs::read_to_string(base.join("projects/app/result")).unwrap(),
        "app\n"
    );
    assert_eq!(
        fs::read_to_string(base.join("build-output/result")).unwrap(),
        "build\n"
    );
    assert!(!base.join("tools").join("new").exists());
}

#[test]
#[ignore = "requires a hypervisor and NVX guest artifacts; run scripts/nvx.py test-aci-edge-sandboxes"]
fn mapped_files_hide_their_siblings_and_names_may_hold_spaces() {
    let host = tempfile::tempdir().unwrap();
    let work = host.path().join("my work");
    fs::create_dir_all(work.join("data").join("My Secrets")).unwrap();
    fs::write(work.join("notes one.txt"), "notes").unwrap();
    fs::write(work.join("log file.txt"), "").unwrap();
    fs::write(work.join("sibling.txt"), "hidden").unwrap();
    fs::write(work.join("data").join("My Secrets").join("key"), "hidden").unwrap();
    fs::write(work.join("data").join("shared.txt"), "shared").unwrap();
    let request = ProvisionRequest::new().with_filesystem(FilesystemPolicy {
        readonly_paths: vec![work.join("notes one.txt")],
        readwrite_paths: vec![work.join("log file.txt"), work.join("data")],
        denied_paths: vec![work.join("data").join("My Secrets")],
    });
    let (nvx, backend) = client("files");
    let sandbox = started(&nvx, &backend, &request);
    let sandbox_id = id(&sandbox);
    let guest = |name: &str| guest_path(&work.join(name)).unwrap();
    let directory = guest_path(&work).unwrap();
    let output = run(
        &nvx,
        sandbox_id,
        script(
            "cat \"$1\" && echo logged >> \"$2\" && ! echo x > \"$1\" 2>/dev/null && ls \"$3\" \
             && ls \"$4\"",
            &[
                guest("notes one.txt"),
                guest("log file.txt"),
                directory.clone(),
                guest("data"),
            ],
        ),
    );
    assert!(output.outcome.success(), "{output:?}");
    // The guest's directory holds only the mapped paths, and the denied directory is hidden.
    assert_eq!(
        output.stdout, b"notesdata\nlog file.txt\nnotes one.txt\nshared.txt\n",
        "{output:?}"
    );
    assert_eq!(
        fs::read_to_string(work.join("log file.txt")).unwrap(),
        "logged\n"
    );
    for hidden in [
        format!("{directory}/sibling.txt"),
        format!("{directory}/data/My Secrets/key"),
    ] {
        let output = run(
            &nvx,
            sandbox_id,
            script("cat \"$1\"", std::slice::from_ref(&hidden)),
        );
        assert_ne!(
            output.outcome,
            ExecOutcome::Exited(0),
            "{hidden}: {output:?}"
        );
    }
}

#[test]
#[ignore = "requires a hypervisor and NVX guest artifacts; run scripts/nvx.py test-aci-edge-sandboxes"]
fn single_files_are_granted_without_their_directories() {
    // The layout of microsoft/nvx#282: a read-only settings file and a read-write output file,
    // each beside a file that the policy does not grant.
    let host = tempfile::tempdir().unwrap();
    let base = host.path();
    for (name, contents) in [
        ("config/settings.json", "settings"),
        ("config/secret.json", "secret"),
        ("results/output.txt", "output"),
        ("results/other.txt", "other"),
    ] {
        let path = base.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    let request = ProvisionRequest::new().with_filesystem(FilesystemPolicy {
        readonly_paths: vec![base.join("config/settings.json")],
        readwrite_paths: vec![base.join("results/output.txt")],
        denied_paths: Vec::new(),
    });
    let (nvx, backend) = client("grants");
    let sandbox = started(&nvx, &backend, &request);
    let sandbox_id = id(&sandbox);
    let guest = |name: &str| guest_path(&base.join(name)).unwrap();
    let output = run(
        &nvx,
        sandbox_id,
        script(
            "cat \"$1\" && ! echo changed > \"$1\" && ! rm -f \"$1\" && echo updated > \"$2\" \
             && cat \"$2\" && ls -a \"$3\" && ls -a \"$4\" && [ ! -e \"$5\" ] && [ ! -e \"$6\" ]",
            &[
                guest("config/settings.json"),
                guest("results/output.txt"),
                guest("config"),
                guest("results"),
                guest("config/secret.json"),
                guest("results/other.txt"),
            ],
        ),
    );
    assert!(output.outcome.success(), "{output:?}");
    // The guest's directories hold only the granted files.
    assert_eq!(
        output.stdout, b"settingsupdated\n.\n..\nsettings.json\n.\n..\noutput.txt\n",
        "{output:?}"
    );
    assert_eq!(
        fs::read_to_string(base.join("config/settings.json")).unwrap(),
        "settings"
    );
    assert_eq!(
        fs::read_to_string(base.join("results/output.txt")).unwrap(),
        "updated\n"
    );
    // Nothing beside the granted files changed, and nothing was created there.
    for (directory, entries) in [
        (
            "config",
            [("secret.json", "secret"), ("settings.json", "settings")],
        ),
        (
            "results",
            [("other.txt", "other"), ("output.txt", "updated\n")],
        ),
    ] {
        let mut found: Vec<(String, String)> = fs::read_dir(base.join(directory))
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.file_name().into_string().unwrap(),
                    fs::read_to_string(entry.path()).unwrap(),
                )
            })
            .collect();
        found.sort();
        let expected: Vec<(String, String)> = entries
            .iter()
            .map(|(name, contents)| ((*name).to_owned(), (*contents).to_owned()))
            .collect();
        assert_eq!(found, expected, "{directory}");
    }
}

#[test]
#[ignore = "requires a hypervisor and NVX guest artifacts; run scripts/nvx.py test-aci-edge-sandboxes"]
fn many_mappings_are_mounted() {
    let host = tempfile::tempdir().unwrap();
    let mut readonly_paths = Vec::new();
    let mut readwrite_paths = Vec::new();
    for index in 0..25 {
        let directory = host.path().join(format!("tree-{index:02}")).join("src");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("file.txt"), format!("{index}")).unwrap();
        let file = host.path().join(format!("file-{index:02}.txt"));
        fs::write(&file, format!("{index}")).unwrap();
        if index % 2 == 0 {
            readonly_paths.push(directory);
            readwrite_paths.push(file);
        } else {
            readwrite_paths.push(directory);
            readonly_paths.push(file);
        }
    }
    let mapped: Vec<String> = readonly_paths
        .iter()
        .chain(&readwrite_paths)
        .map(|path| guest_path(path).unwrap())
        .collect();
    assert_eq!(mapped.len(), 50);
    let request = ProvisionRequest::new().with_filesystem(FilesystemPolicy {
        readonly_paths,
        readwrite_paths,
        denied_paths: Vec::new(),
    });
    let (nvx, backend) = client("many");
    let sandbox = started(&nvx, &backend, &request);
    let output = run(
        &nvx,
        id(&sandbox),
        script(
            "count=0; for path in \"$@\"; do [ -e \"$path\" ] || exit 1; \
             count=$((count + 1)); done; echo $count",
            &mapped,
        ),
    );
    assert_eq!(output.stdout, b"50\n", "{output:?}");
}

#[test]
#[ignore = "requires a hypervisor and NVX guest artifacts; run scripts/nvx.py test-aci-edge-sandboxes"]
fn directories_on_another_volume_are_mapped() {
    let Some(volume) = second_volume() else {
        eprintln!("skipped: the host has no second volume");
        return;
    };
    let primary = tempfile::tempdir().unwrap();
    let secondary = tempfile::tempdir_in(volume).unwrap();
    fs::write(primary.path().join("input.txt"), "input").unwrap();
    let request = ProvisionRequest::new().with_filesystem(FilesystemPolicy {
        readonly_paths: vec![primary.path().to_path_buf()],
        readwrite_paths: vec![secondary.path().to_path_buf()],
        denied_paths: Vec::new(),
    });
    let (nvx, backend) = client("volume");
    let sandbox = started(&nvx, &backend, &request);
    let output = run(
        &nvx,
        id(&sandbox),
        script(
            "cat \"$1/input.txt\" > \"$2/output.txt\"",
            &[
                guest_path(primary.path()).unwrap(),
                guest_path(secondary.path()).unwrap(),
            ],
        ),
    );
    assert!(output.outcome.success(), "{output:?}");
    assert_eq!(
        fs::read_to_string(secondary.path().join("output.txt")).unwrap(),
        "input"
    );
}

/// Runs the shell's `pwd`, which reports `PWD`, then `/bin/pwd`, which resolves the directory.
fn pwd(nvx: &AciEdgeSandbox, sandbox_id: &SandboxId, cwd: Option<&str>) -> ExecOutput {
    let request = ExecRequest::command_line("pwd; /bin/pwd");
    let request = match cwd {
        Some(cwd) => request.with_cwd(cwd),
        None => request,
    };
    run(nvx, sandbox_id, request)
}

#[test]
#[ignore = "requires a hypervisor and NVX guest artifacts; run scripts/nvx.py test-aci-edge-sandboxes"]
fn working_directories_apply_to_each_execution() {
    let (nvx, backend) = client("cwd");
    let sandbox = started(&nvx, &backend, &ProvisionRequest::new());
    let sandbox_id = id(&sandbox);

    // Without a working directory, a workload starts in the guest's root directory.
    let default = pwd(&nvx, sandbox_id, None);
    assert_eq!(default.stdout, b"/\n/\n", "{default:?}");
    let setup = shell(
        &nvx,
        sandbox_id,
        "mkdir -p /tmp/work/a /tmp/work/b /tmp/work/locked && touch /tmp/work/file && \
         chmod 000 /tmp/work/locked && ln -s /tmp/work/a /tmp/work/link",
    );
    assert!(setup.outcome.success(), "{setup:?}");

    // Each execution starts in the directory it names, and none carries over to the next.
    for (cwd, expected) in [
        (Some("/tmp/work/a"), "/tmp/work/a\n/tmp/work/a\n"),
        (Some("/tmp/work/b"), "/tmp/work/b\n/tmp/work/b\n"),
        (None, "/\n/\n"),
        (Some("/tmp/work/a"), "/tmp/work/a\n/tmp/work/a\n"),
        // PWD keeps the name the caller gave, while /bin/pwd resolves the link.
        (Some("/tmp/work/link"), "/tmp/work/link\n/tmp/work/a\n"),
        // A name with empty or "." components reports the directory it resolves to.
        (Some("/tmp//work/./b/"), "/tmp/work/b\n/tmp/work/b\n"),
    ] {
        let output = pwd(&nvx, sandbox_id, cwd);
        assert_eq!(
            output.outcome,
            ExecOutcome::Exited(0),
            "{cwd:?}: {output:?}"
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            expected,
            "{cwd:?}: {output:?}"
        );
    }
    // An argument vector starts in the working directory as well.
    let listing = run(
        &nvx,
        sandbox_id,
        ExecRequest::argv(["/bin/ls"]).with_cwd("/tmp/work"),
    );
    assert_eq!(listing.stdout, b"a\nb\nfile\nlink\nlocked\n", "{listing:?}");

    // A directory that the workload cannot enter fails the launch, and nothing runs elsewhere.
    for (cwd, reason) in [
        ("/tmp/work/missing", "No such file or directory"),
        ("/tmp/work/file", "Not a directory"),
        ("/tmp/work/locked", "Permission denied"),
        // Root could enter /root, but the workload's identity cannot.
        ("/root", "Permission denied"),
    ] {
        let output = run(
            &nvx,
            sandbox_id,
            ExecRequest::command_line("touch /tmp/work/ran").with_cwd(cwd),
        );
        assert_eq!(
            output.outcome,
            ExecOutcome::Failed(ExecFailure::WorkingDirectory),
            "{cwd}: {output:?}"
        );
        assert!(output.stdout.is_empty(), "{cwd}: {output:?}");
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        assert!(
            diagnostic.contains(&format!("working directory {cwd}: {reason}")),
            "{cwd}: {diagnostic}"
        );
    }
    let relative = nvx
        .exec(
            sandbox_id,
            &ExecRequest::command_line("touch /tmp/work/ran").with_cwd("tmp/work"),
        )
        .unwrap_err();
    assert_eq!(relative.code(), ErrorCode::PolicyValidation, "{relative}");
    assert!(
        shell(&nvx, sandbox_id, "test ! -e /tmp/work/ran")
            .outcome
            .success()
    );

    // Refused launches leave the sandbox usable.
    assert_eq!(
        pwd(&nvx, sandbox_id, Some("/tmp/work/b")).stdout,
        b"/tmp/work/b\n/tmp/work/b\n"
    );
}

fn sorted<const N: usize>(lines: [&str; N]) -> Vec<String> {
    let mut lines = lines.map(str::to_owned).to_vec();
    lines.sort();
    lines
}

/// Returns the sorted `NAME=VALUE` lines that the workload of `request` prints.
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
    // Programs run directly, because a shell exports variables of its own.
    let env = || ExecRequest::argv(["/usr/bin/env"]);
    let listing = |request| environment(&nvx, sandbox_id, request);

    // No environment supplied: the guest's default environment, which never holds host variables.
    let default = listing(env());
    for expected in [
        "PATH=/usr/sbin:/usr/bin:/sbin:/bin",
        "TERM=linux",
        "HOME=/",
        "USER=nobody",
        "LOGNAME=nobody",
    ] {
        assert!(default.iter().any(|line| line == expected), "{default:?}");
    }
    assert!(
        !default
            .iter()
            .any(|line| line.starts_with("ACI_EDGE_SANDBOXES_") || line.starts_with("NVX_")),
        "{default:?}"
    );
    // `inheritDefaultEnv` selects nothing without entries.
    for inherit in [true, false] {
        assert_eq!(listing(env().with_inherit_default_env(inherit)), default);
    }

    // An explicitly empty environment starts the workload without any variable.
    for request in [
        env().with_envs(Vec::<String>::new()),
        env()
            .with_envs(Vec::<String>::new())
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
                .with_envs(Vec::<String>::new())
                .with_inherit_default_env(true)
        ),
        default
    );

    // Entries are the whole environment, and an empty value stays an empty value.
    assert_eq!(
        listing(env().with_envs(["FOO=bar", "EMPTY="])),
        ["EMPTY=", "FOO=bar"]
    );
    assert_eq!(
        listing(env().with_env("FOO=bar").with_inherit_default_env(false)),
        ["FOO=bar"]
    );
    // The guest agent refuses a name that repeats, so the request fails before anything runs.
    let repeated = nvx
        .exec(sandbox_id, &env().with_envs(["A=1", "B=2", "A=3"]))
        .unwrap_err();
    assert_eq!(repeated.code(), ErrorCode::PolicyValidation, "{repeated}");
    // A shell receives the entries too, and keeps none of the default environment.
    let output = run(
        &nvx,
        sandbox_id,
        ExecRequest::command_line(
            "printf '%s|%s|%s' \"$FOO\" \"${EMPTY-unset}\" \"${HOME-unset}\"",
        )
        .with_envs(["FOO=bar", "EMPTY="]),
    );
    assert_eq!(output.stdout, b"bar||unset", "{output:?}");

    // Layered entries come on top of the default environment, and an entry replaces a default.
    let layered = |entries: &[&str]| -> Vec<String> {
        let mut expected: Vec<String> = default
            .iter()
            .filter(|line| {
                let name = line.split_once('=').unwrap().0;
                !entries
                    .iter()
                    .any(|entry| entry.split_once('=').unwrap().0 == name)
            })
            .cloned()
            .chain(entries.iter().map(|entry| (*entry).to_owned()))
            .collect();
        expected.sort();
        expected
    };
    assert_eq!(
        listing(
            env()
                .with_envs(["FOO=bar", "PATH=/custom"])
                .with_inherit_default_env(true)
        ),
        layered(&["FOO=bar", "PATH=/custom"])
    );

    // The guest agent enters a working directory itself, so no shell adds `PWD`, `OLDPWD`, or
    // `SHLVL` to the environment of a program or rewrites entries with these names.
    let in_tmp = || env().with_cwd("/tmp");
    assert_eq!(listing(in_tmp().with_env("FOO=bar")), ["FOO=bar"]);
    assert_eq!(
        listing(in_tmp().with_envs(Vec::<String>::new())),
        Vec::<String>::new()
    );
    let shell_names = [
        "PWD=/custom",
        "SHLVL=7",
        "OLDPWD=/keep",
        "-DASHED=1",
        "A.B=2",
        "FOO=bar",
    ];
    assert_eq!(
        listing(in_tmp().with_envs(shell_names)),
        sorted(shell_names)
    );
    let layered_names = ["PWD=/custom", "SHLVL=7", "FOO=bar"];
    assert_eq!(
        listing(
            in_tmp()
                .with_envs(layered_names)
                .with_inherit_default_env(true)
        ),
        layered(&layered_names)
    );
    // A command line runs in a shell, which is the workload, so it exports `SHLVL` and `PWD`
    // of its own, replacing entries with these names.
    let shell = || ExecRequest::command_line("env");
    assert_eq!(
        listing(shell().with_env("FOO=bar")),
        sorted(["FOO=bar", "PWD=/", "SHLVL=1"])
    );
    assert_eq!(
        listing(
            shell()
                .with_cwd("/tmp")
                .with_envs(["FOO=bar", "SHLVL=7", "PWD=/custom"])
        ),
        sorted(["FOO=bar", "PWD=/tmp", "SHLVL=8"])
    );
    // BusyBox reads `SHLVL` with `atoi` and exports one more as an unsigned integer.
    for (received, exported) in [
        ("SHLVL=4294967295", "SHLVL=0"),
        ("SHLVL=-3", "SHLVL=4294967294"),
        ("SHLVL=7x", "SHLVL=8"),
        ("SHLVL=abc", "SHLVL=1"),
    ] {
        assert_eq!(
            listing(shell().with_env(received)),
            sorted(["PWD=/", exported])
        );
    }

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
    let (nvx, backend) = client("env-values");
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
            ExecRequest::argv(["/bin/printenv", "--", name]).with_envs(entries()),
        );
        assert_eq!(program.stdout, format!("{value}\n").as_bytes(), "{name}");
        // The workload need not be a program: a shell reads the same values.
        if !name.starts_with('-') {
            let shell = run(
                &nvx,
                sandbox_id,
                ExecRequest::command_line(format!("printf '%s' \"${name}\"")).with_envs(entries()),
            );
            assert_eq!(shell.stdout, value.as_bytes(), "{name}");
        }
    }
    // A program lists the entries in the order in which they were supplied, also in a working
    // directory.
    let listing: String = values
        .iter()
        .map(|(name, value)| format!("{name}={value}\n"))
        .collect();
    for request in [
        ExecRequest::argv(["/usr/bin/env"]),
        ExecRequest::argv(["/usr/bin/env"]).with_cwd("/tmp"),
    ] {
        let all = run(&nvx, sandbox_id, request.with_envs(entries()));
        assert_eq!(all.stdout, listing.as_bytes(), "{all:?}");
    }

    // The guest agent takes 256 entries of 4096 bytes each.
    let many: Vec<String> = (0..256).map(|index| format!("V{index}=x")).collect();
    let full = run(
        &nvx,
        sandbox_id,
        ExecRequest::argv(["/usr/bin/env"]).with_envs(many.clone()),
    );
    let expected: String = many.iter().map(|entry| format!("{entry}\n")).collect();
    assert_eq!(full.stdout, expected.as_bytes(), "{full:?}");
    let large = format!("BIG={}", "x".repeat(4096 - 4));
    let output = run(
        &nvx,
        sandbox_id,
        ExecRequest::argv(["/usr/bin/env"]).with_env(large.clone()),
    );
    assert_eq!(output.stdout, format!("{large}\n").as_bytes());
    for beyond in [
        ExecRequest::argv(["/usr/bin/env"]).with_envs((0..257).map(|index| format!("V{index}=x"))),
        ExecRequest::argv(["/usr/bin/env"]).with_env(format!("BIG={}", "x".repeat(4096 - 3))),
    ] {
        let rejected = nvx.exec(sandbox_id, &beyond).unwrap_err();
        assert_eq!(rejected.code(), ErrorCode::PolicyValidation, "{rejected}");
    }
}

#[test]
#[ignore = "requires a hypervisor and NVX guest artifacts; run scripts/nvx.py test-aci-edge-sandboxes"]
fn environments_leave_the_workload_contained() {
    let (nvx, backend) = client("containment");
    let sandbox = started(&nvx, &backend, &ProvisionRequest::new());
    let sandbox_id = id(&sandbox);

    // The guest agent enters the working directory with the workload's identity and applies the
    // entries after it dropped the workload's privileges, so the workload keeps its identity, no
    // capabilities, and `no_new_privs`.
    let probe =
        || ExecRequest::command_line("id -u; grep -E '^(CapEff|NoNewPrivs):' /proc/self/status");
    for request in [
        probe(),
        probe().with_env("FOO=bar"),
        probe().with_envs(Vec::<String>::new()),
        probe().with_env("FOO=bar").with_inherit_default_env(true),
        probe().with_cwd("/tmp").with_env("FOO=bar"),
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
    // A directory that is missing, or that the workload identity cannot enter, fails the launch
    // before the workload runs.
    for cwd in ["/missing", "/root"] {
        let refused = run(
            &nvx,
            sandbox_id,
            ExecRequest::argv(["/bin/true"])
                .with_cwd(cwd)
                .with_env("FOO=bar"),
        );
        assert_eq!(
            refused.outcome,
            ExecOutcome::Failed(ExecFailure::WorkingDirectory),
            "{refused:?}"
        );
    }

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
            &ExecRequest::command_line("echo started; sleep 30").with_envs(["FOO=bar"]),
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

/// Reports whether TCP reaches the portable profile's DNS service on the guest's gateway. Probing
/// it needs no Internet access, so the tests that use it are deterministic.
const GATEWAY_TCP_PROBE: &str = "nc -w 3 10.0.0.1 53 </dev/null && echo reached || echo blocked";

#[test]
#[ignore = "requires a hypervisor and NVX guest artifacts; run scripts/nvx.py test-aci-edge-sandboxes"]
fn network_policies_are_enforced() {
    let (nvx, backend) = client("network");
    let excluding_gateway = || NetworkRule {
        to: vec![NetworkPeer {
            cidr: "10.0.0.0/24".to_owned(),
            except: vec!["10.0.0.1/32".to_owned()],
        }],
        ports: Vec::new(),
    };
    let cases: [(&str, NetworkPolicy, &[u8]); 8] = [
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
            "allow exclusion is rule local",
            NetworkPolicy {
                egress: EgressPolicy::new(Access::Deny)
                    .with_allow(excluding_gateway())
                    .with_allow(NetworkRule::to("10.0.0.1").on_port(Protocol::Tcp, 53)),
                ..NetworkPolicy::deny_all()
            },
            b"reached\n",
        ),
        (
            "deny exclusion follows default",
            NetworkPolicy {
                egress: EgressPolicy::new(Access::Allow).with_deny(excluding_gateway()),
                ..NetworkPolicy::deny_all()
            },
            b"reached\n",
        ),
        (
            "explicit deny takes precedence",
            NetworkPolicy {
                egress: EgressPolicy::new(Access::Deny)
                    .with_allow(NetworkRule::to("10.0.0.1").on_port(Protocol::Tcp, 53))
                    .with_deny(NetworkRule::to("10.0.0.1")),
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
            GATEWAY_TCP_PROBE
        };
        let output = shell(&nvx, id(&sandbox), command);
        assert_eq!(output.stdout, expected, "{name}: {output:?}");
    }
}

#[test]
#[ignore = "requires a hypervisor and NVX guest artifacts; run scripts/nvx.py test-aci-edge-sandboxes"]
fn network_protocol_selectors_are_enforced() {
    // Unprivileged workloads cannot send ICMP, and UDP replies from the gateway's DNS service
    // depend on the host resolver, so every selector is observed through the TCP probe: a rule
    // for another protocol must neither admit nor block it.
    let (nvx, backend) = client("protocol");
    let gateway = || NetworkRule::to("10.0.0.1");
    let allow = |rule: NetworkRule| NetworkPolicy {
        egress: EgressPolicy::new(Access::Deny).with_allow(rule),
        ..NetworkPolicy::deny_all()
    };
    // The address-only allow rule admits the probe, so only deny precedence can block it.
    let deny = |rule: NetworkRule| NetworkPolicy {
        egress: EgressPolicy::new(Access::Deny)
            .with_allow(gateway())
            .with_deny(rule),
        ..NetworkPolicy::deny_all()
    };
    let cases: [(&str, NetworkPolicy, &[u8]); 10] = [
        (
            "allow tcp",
            allow(gateway().on_protocol(Protocol::Tcp)),
            b"reached\n",
        ),
        (
            "allow udp",
            allow(gateway().on_protocol(Protocol::Udp)),
            b"blocked\n",
        ),
        (
            "allow icmp",
            allow(gateway().on_protocol(Protocol::Icmp)),
            b"blocked\n",
        ),
        (
            "allow any on the port",
            allow(gateway().on_port(Protocol::Any, 53)),
            b"reached\n",
        ),
        (
            "allow any on another port",
            allow(gateway().on_port(Protocol::Any, 54)),
            b"blocked\n",
        ),
        (
            "deny tcp",
            deny(gateway().on_protocol(Protocol::Tcp)),
            b"blocked\n",
        ),
        (
            "deny udp",
            deny(gateway().on_protocol(Protocol::Udp)),
            b"reached\n",
        ),
        (
            "deny icmp",
            deny(gateway().on_protocol(Protocol::Icmp)),
            b"reached\n",
        ),
        (
            "deny any on the port",
            deny(gateway().on_port(Protocol::Any, 53)),
            b"blocked\n",
        ),
        (
            "deny any on another port",
            deny(gateway().on_port(Protocol::Any, 54)),
            b"reached\n",
        ),
    ];
    for (name, policy, expected) in cases {
        let sandbox = started(
            &nvx,
            &backend,
            &ProvisionRequest::new().with_network(policy),
        );
        let output = shell(&nvx, id(&sandbox), GATEWAY_TCP_PROBE);
        assert_eq!(output.stdout, expected, "{name}: {output:?}");
    }
}

/// Reports, on one line each, whether TCP reaches the gateway's DNS service over IPv4 and over
/// IPv6. OpenVMM derives the guest's IPv6 network from its IPv4 one, so the default
/// `10.0.0.2/24` makes the IPv6 gateway `fd00::a00:1`.
const GATEWAY_DUAL_STACK_PROBE: &str = "for gateway in 10.0.0.1 fd00::a00:1; do \
     nc -w 3 $gateway 53 </dev/null && echo reached || echo blocked; done";

#[test]
#[ignore = "requires a hypervisor and NVX guest artifacts; run scripts/nvx.py test-aci-edge-sandboxes"]
fn ipv6_network_policies_are_enforced() {
    let (nvx, backend) = client("ipv6");
    let egress = |egress: EgressPolicy| NetworkPolicy {
        egress,
        ..NetworkPolicy::deny_all()
    };
    let deny = || EgressPolicy::new(Access::Deny);
    let cases: [(&str, NetworkPolicy, &[u8]); 8] = [
        (
            "allow",
            NetworkPolicy::egress(Access::Allow),
            b"reached\nreached\n",
        ),
        (
            "ipv6 allow rule",
            egress(deny().with_allow(NetworkRule::to("fd00::a00:1").on_port(Protocol::Tcp, 53))),
            b"blocked\nreached\n",
        ),
        (
            "ipv4 allow rule",
            egress(deny().with_allow(NetworkRule::to("10.0.0.1").on_port(Protocol::Tcp, 53))),
            b"reached\nblocked\n",
        ),
        (
            "ipv6 wildcard",
            egress(deny().with_allow(NetworkRule::to("::/0"))),
            b"blocked\nreached\n",
        ),
        (
            "rule without destinations",
            egress(deny().with_allow(NetworkRule::default().on_port(Protocol::Any, 53))),
            b"reached\nreached\n",
        ),
        (
            "other ipv6 allow rule",
            egress(deny().with_allow(NetworkRule::to("2001:db8::/32"))),
            b"blocked\nblocked\n",
        ),
        (
            "ipv6 deny precedence",
            egress(
                deny()
                    .with_allow(NetworkRule::to("::/0"))
                    .with_allow(NetworkRule::to("10.0.0.1"))
                    .with_deny(NetworkRule::to("fd00::a00:1").on_protocol(Protocol::Tcp)),
            ),
            b"reached\nblocked\n",
        ),
        (
            "ipv6 deny rule",
            egress(EgressPolicy::new(Access::Allow).with_deny(NetworkRule::to("fd00::/8"))),
            b"reached\nblocked\n",
        ),
    ];
    for (name, policy, expected) in cases {
        let sandbox = started(
            &nvx,
            &backend,
            &ProvisionRequest::new().with_network(policy),
        );
        let output = shell(&nvx, id(&sandbox), GATEWAY_DUAL_STACK_PROBE);
        assert_eq!(output.stdout, expected, "{name}: {output:?}");
    }
}

#[test]
#[ignore = "requires a hypervisor and NVX guest artifacts; run scripts/nvx.py test-aci-edge-sandboxes"]
fn network_port_ranges_are_enforced() {
    // The TCP probe targets port 53 of the gateway, so a range admits or blocks it when the range
    // contains port 53: at its first or last port, or inside it, but not from the next port up
    // or through the next port down, and a UDP range never does.
    let (nvx, backend) = client("ranges");
    let gateway = || NetworkRule::to("10.0.0.1");
    let allow = |rule: NetworkRule| NetworkPolicy {
        egress: EgressPolicy::new(Access::Deny).with_allow(rule),
        ..NetworkPolicy::deny_all()
    };
    // The address-only allow rule admits the probe, so only deny precedence can block it.
    let deny = |rule: NetworkRule| NetworkPolicy {
        egress: EgressPolicy::new(Access::Deny)
            .with_allow(gateway())
            .with_deny(rule),
        ..NetworkPolicy::deny_all()
    };
    let cases: [(&str, NetworkPolicy, &[u8]); 10] = [
        (
            "allow a tcp range from the port",
            allow(gateway().on_port_range(Protocol::Tcp, 53, 60)),
            b"reached\n",
        ),
        (
            "allow a tcp range through the port",
            allow(gateway().on_port_range(Protocol::Tcp, 40, 53)),
            b"reached\n",
        ),
        (
            "allow an any range around the port",
            allow(gateway().on_port_range(Protocol::Any, 50, 60)),
            b"reached\n",
        ),
        (
            "allow a tcp range from the next port",
            allow(gateway().on_port_range(Protocol::Tcp, 54, 60)),
            b"blocked\n",
        ),
        (
            "allow a tcp range through the previous port",
            allow(gateway().on_port_range(Protocol::Tcp, 40, 52)),
            b"blocked\n",
        ),
        (
            "allow every udp port",
            allow(gateway().on_port_range(Protocol::Udp, 1, 65535)),
            b"blocked\n",
        ),
        (
            "allow a tcp range around a denied port",
            NetworkPolicy {
                egress: EgressPolicy::new(Access::Deny)
                    .with_allow(gateway().on_port_range(Protocol::Tcp, 50, 60))
                    .with_deny(gateway().on_port(Protocol::Tcp, 53)),
                ..NetworkPolicy::deny_all()
            },
            b"blocked\n",
        ),
        (
            "deny a tcp range around the port",
            deny(gateway().on_port_range(Protocol::Tcp, 50, 60)),
            b"blocked\n",
        ),
        (
            "deny a tcp range from the next port",
            deny(gateway().on_port_range(Protocol::Tcp, 54, 60)),
            b"reached\n",
        ),
        (
            "deny every udp port",
            deny(gateway().on_port_range(Protocol::Udp, 1, 65535)),
            b"reached\n",
        ),
    ];
    for (name, policy, expected) in cases {
        let sandbox = started(
            &nvx,
            &backend,
            &ProvisionRequest::new().with_network(policy),
        );
        let output = shell(&nvx, id(&sandbox), GATEWAY_TCP_PROBE);
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
    // The guest agent accepts every timeout that MXC allows, far beyond an hour, and a workload
    // that ends first finishes normally.
    for millis in [3_600_001, 86_400_000, ProcessSpec::MAX_TIMEOUT_MS] {
        let output = run(
            &nvx,
            &sandbox_id,
            ExecRequest::command_line("sleep 1; echo done")
                .with_timeout(Duration::from_millis(millis)),
        );
        assert_eq!(
            (output.outcome, output.stdout.as_slice()),
            (ExecOutcome::Exited(0), &b"done\n"[..]),
            "{millis} ms"
        );
    }
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
