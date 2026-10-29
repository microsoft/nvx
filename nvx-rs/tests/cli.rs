//! The tests of the `nvx-rs` CLI, which run it in fresh artifact roots with `nvx-fake-openvmm` as
//! OpenVMM.
//!
//! The scenario tests of `tests/cli/` check what callers of `nvx-rs` rely on, as the [`harness`]
//! observes it: exit statuses, errors by their codes and stable fragments, the entries of state
//! directories and their records, parsed, POSIX file modes, and the arguments that OpenVMM
//! receives. Their tables cover usage errors and help ([`usage`]), the checks that refuse a command
//! line before anything changes ([`validation`]), dry runs and the splitting of their `>> ` lines
//! ([`dry_run`]), one-shot runs ([`one_shot`]), the lifecycles of managed sandboxes, their
//! executions, and the refusals of their operations ([`lifecycle`]), the configurations of every
//! format ([`formats`]), `egress compile` ([`egress`]), `artifacts` ([`artifacts`]), and `download`
//! and the download that `run` and `sandbox` start themselves, without a network ([`download`]).
//! They replaced the scenarios that scripts recorded from the Python implementations of
//! `nvx.py sandbox` and `nvx.py run`, and that this test replayed until microsoft/nvx#728 retired
//! them.
//!
//! The other tests check what `nvx-rs` adds to `nvx.py`.
#![cfg(all(
    feature = "cli",
    feature = "testing",
    any(target_os = "linux", windows)
))]

#[path = "cli/artifacts.rs"]
mod artifacts;
#[path = "cli/download.rs"]
mod download;
#[path = "cli/dry_run.rs"]
mod dry_run;
#[path = "cli/egress.rs"]
mod egress;
#[path = "cli/formats.rs"]
mod formats;
#[path = "cli/harness.rs"]
mod harness;
#[path = "cli/lifecycle.rs"]
mod lifecycle;
#[path = "cli/one_shot.rs"]
mod one_shot;
#[path = "cli/openvmm.rs"]
mod openvmm;
#[path = "cli/usage.rs"]
mod usage;
#[path = "cli/validation.rs"]
mod validation;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use nvx::host::process::{Process, ProcessRef};
use serde_json::{Value, json};

use self::harness::{FAKE_OPENVMM, NVX_RS, Root, canonical};

/// Runs `nvx-rs --root ROOT ARGUMENTS` in the root, without the variables that change its
/// output.
fn nvx_rs(root: &Root, arguments: &[String], environment: &BTreeMap<String, String>) -> Output {
    root.command(arguments).envs(environment).output().unwrap()
}

/// Returns the arguments that provision a minimal sandbox in `state` below the root.
fn provision(root: &Root, state: &str, command_line: &str) -> Vec<String> {
    provision_layer(root, state, &root.path.join("distro.erofs"), command_line)
}

/// Returns the arguments that provision a minimal sandbox of the distro layer `layer` in `state`
/// below the root.
fn provision_layer(root: &Root, state: &str, layer: &Path, command_line: &str) -> Vec<String> {
    let path = |name: &str| root.path.join(name).to_str().unwrap().to_owned();
    vec![
        "sandbox".into(),
        "provision".into(),
        "--state-dir".into(),
        path(state),
        "--layer".into(),
        format!(
            "distro,{},11111111-1111-1111-1111-111111111111",
            layer.to_str().unwrap()
        ),
        "--scratch".into(),
        path("scratch.ext4"),
        "--cmdline".into(),
        command_line.into(),
    ]
}

#[cfg(unix)]
#[test]
fn roots_link_the_built_fake_openvmm() {
    let root = Root::new();
    let executable = root.below(harness::OPENVMM);
    assert_eq!(fs::read_link(executable).unwrap(), Path::new(FAKE_OPENVMM));
}

fn run(root: &Root, arguments: &[String]) -> Output {
    nvx_rs(root, arguments, &BTreeMap::new())
}

fn lifecycle(operation: &str, root: &Root, state: &str) -> Vec<String> {
    vec![
        "sandbox".into(),
        operation.into(),
        "--state-dir".into(),
        root.path.join(state).to_str().unwrap().to_owned(),
    ]
}

fn text(data: &[u8]) -> String {
    String::from_utf8_lossy(data).replace("\r\n", "\n")
}

#[test]
fn start_requires_the_guest_features_of_layered_sandboxes() {
    let root = Root::new();
    // A guest that lacks per-execution environments or host path mapping tables, or that
    // predates feature negotiation, is stopped, and its start fails like any other.
    for (state, command_line, missing) in [
        (
            "cancel-only",
            "fake_guest_features=1",
            "per-execution environments, host path mapping tables",
        ),
        (
            "legacy",
            "fake_legacy_guest=1",
            "per-execution environments, host path mapping tables",
        ),
        (
            "environments-only",
            "fake_guest_features=16",
            "host path mapping tables",
        ),
    ] {
        let output = run(&root, &provision(&root, state, command_line));
        assert!(output.status.success(), "{}", text(&output.stderr));
        let output = run(&root, &lifecycle("start", &root, state));
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            text(&output.stderr),
            format!(
                "error[guest-features-missing]: the guest image lacks control features that \
                 layered sandboxes need ({missing}); use an NVX release or build that includes \
                 them\n"
            )
        );
        let mut names: Vec<String> = fs::read_dir(root.path.join(state))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["config.json", "openvmm.log", "sandbox.lock"]);
        let output = run(&root, &lifecycle("deprovision", &root, state));
        assert!(output.status.success(), "{}", text(&output.stderr));
    }
    // Per-execution environments and host path mapping tables are all that the CLI needs:
    // neither cancellation nor workload accounts.
    let output = run(
        &root,
        &provision(&root, "environments", "fake_guest_features=80"),
    );
    assert!(output.status.success(), "{}", text(&output.stderr));
    for operation in ["start", "stop", "deprovision"] {
        let output = run(&root, &lifecycle(operation, &root, "environments"));
        assert!(
            output.status.success(),
            "{operation}: {}",
            text(&output.stderr)
        );
    }
}

#[test]
fn a_mapping_that_the_guest_refuses_leaves_no_usable_start() {
    // The guest agent mounts each share as the mapping table that start sends once it is ready
    // orders, and refuses workloads until then: a mapping that it cannot mount ends the start.
    // The guest then shuts down as a stop has it do, and OpenVMM reports a stopped VM, unless
    // the guest ignores the request, and OpenVMM is terminated without a report.
    let root = Root::new();
    let share = root.path.join("share");
    for (state, command_line, stops) in [
        ("refused", "fake_refuse_maps=1", true),
        (
            "unstoppable",
            "fake_refuse_maps=1 fake_ignore_stop=1",
            false,
        ),
    ] {
        let mut arguments = provision(&root, state, command_line);
        arguments.extend([
            "--mount".to_owned(),
            format!("/data,{}", share.to_str().unwrap()),
        ]);
        let output = run(&root, &arguments);
        assert!(output.status.success(), "{}", text(&output.stderr));
        let mut start = lifecycle("start", &root, state);
        start.extend(["--timeout".to_owned(), "2".to_owned()]);
        let output = run(&root, &start);
        assert_eq!(output.status.code(), Some(1), "{state}");
        assert_eq!(
            text(&output.stderr),
            error_line(
                "protocol",
                "managed guest could not mount a mapped host path (error 13)"
            )
        );
        let mut names: Vec<String> = fs::read_dir(root.path.join(state))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        if stops {
            assert_eq!(
                names,
                ["config.json", "openvmm.log", "outcome.json", "sandbox.lock"]
            );
            let report: Value = serde_json::from_str(
                &fs::read_to_string(root.path.join(state).join("outcome.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(report["outcome"]["operation"], "managed");
            assert_eq!(report["outcome"]["category"], "success");
        } else {
            assert_eq!(names, ["config.json", "openvmm.log", "sandbox.lock"]);
        }
        assert!(
            !root
                .path
                .join("build")
                .join(format!("fake-openvmm-{state}-maps.json"))
                .exists()
        );
        let mut exec = lifecycle("exec", &root, state);
        exec.extend(["--entrypoint".to_owned(), "/bin/true".to_owned()]);
        let output = run(&root, &exec);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            text(&output.stderr),
            error_line("sandbox-not-running", "sandbox is not running")
        );
        let output = run(&root, &lifecycle("deprovision", &root, state));
        assert!(output.status.success(), "{}", text(&output.stderr));
    }
}

#[test]
fn an_error_after_a_workloads_output_starts_a_line_of_its_own() {
    // The workload leaves its standard error without a line end and creates the outcome report's
    // destination, as one that writes a read-write share could, so the report cannot be published
    // after its output: the error starts a line of its own, where callers find errors.
    let root = Root::new();
    provisioned_with(&root, "unended", "");
    let output = run(&root, &lifecycle("start", &root, "unended"));
    assert!(output.status.success(), "{}", text(&output.stderr));
    let report = root.path.join("exec.json");
    let mut exec = lifecycle("exec", &root, "unended");
    exec.extend([
        "--outcome-report".to_owned(),
        report.to_str().unwrap().to_owned(),
        "--entrypoint".to_owned(),
        "/bin/sh".to_owned(),
        "--arg=-c".to_owned(),
        "--arg".to_owned(),
        format!("printerr progress; touch {}", report.to_str().unwrap()),
    ]);
    let output = run(&root, &exec);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output.stderr));
    assert!(output.stdout.is_empty());
    let stderr = text(&output.stderr);
    let (workload, error) = stderr.split_once('\n').unwrap();
    assert_eq!(workload, "progress");
    assert!(
        error.starts_with("error[outcome-report-exists]: outcome report already exists: "),
        "{stderr}"
    );
    assert!(error.ends_with("exec.json\n"), "{stderr}");
    for operation in ["stop", "deprovision"] {
        let output = run(&root, &lifecycle(operation, &root, "unended"));
        assert!(
            output.status.success(),
            "{operation}: {}",
            text(&output.stderr)
        );
    }
}

#[test]
fn an_error_is_one_line_whatever_the_values_that_its_message_names() {
    // A line break in a value that a message names, here the path of OpenVMM, prints escaped, so
    // that no line of the message reads as another error.
    let root = Root::new();
    let path = |name: &str| root.path.join(name).to_str().unwrap().to_owned();
    let output = Command::new(NVX_RS)
        .arg("--root")
        .arg(&root.path)
        .args([
            "sandbox".to_owned(),
            "run".to_owned(),
            "--layer".to_owned(),
            format!(
                "distro,{},11111111-1111-1111-1111-111111111111",
                path("distro.erofs")
            ),
            "--scratch".to_owned(),
            path("scratch.ext4"),
        ])
        .env("NVX_OPENVMM", path("alt\nerror[unsupported]: openvmm"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = text(&output.stderr);
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(
        stderr.starts_with("error[artifact-missing]: OpenVMM release binary not found: "),
        "{stderr}"
    );
    assert!(
        stderr.ends_with("alt\\nerror[unsupported]: openvmm\n"),
        "{stderr}"
    );
}

#[test]
fn exec_and_stop_tell_a_directory_without_a_sandbox_from_a_stopped_sandbox() {
    // A state directory without the configuration of a sandbox holds no provisioned one, as start
    // finds too, whether it is empty or only the configuration is gone, while a provisioned
    // sandbox that is stopped is merely not running.
    let root = Root::new();
    for state in ["empty", "unconfigured", "stopped"] {
        provisioned_with(&root, state, "");
    }
    for entry in fs::read_dir(root.path.join("empty")).unwrap() {
        fs::remove_file(entry.unwrap().path()).unwrap();
    }
    fs::remove_file(root.path.join("unconfigured").join("config.json")).unwrap();
    for operation in ["exec", "stop"] {
        let invoke = |state: &str| {
            let mut arguments = lifecycle(operation, &root, state);
            if operation == "exec" {
                arguments.extend(["--entrypoint".to_owned(), "/bin/true".to_owned()]);
            }
            let output = run(&root, &arguments);
            assert_eq!(output.status.code(), Some(1), "{operation} {state}");
            text(&output.stderr)
        };
        for state in ["empty", "unconfigured"] {
            let stderr = invoke(state);
            assert!(
                stderr.starts_with(
                    "error[sandbox-not-provisioned]: sandbox configuration not found: "
                ),
                "{operation} {state}: {stderr}"
            );
            assert!(
                stderr.ends_with("config.json\n"),
                "{operation} {state}: {stderr}"
            );
        }
        assert_eq!(
            invoke("stopped"),
            error_line("sandbox-not-running", "sandbox is not running"),
            "{operation}"
        );
    }
}

#[test]
fn a_sandbox_whose_configuration_names_another_layer_role_does_not_start() {
    // A sandbox takes only the distro layer, so a configuration whose layers name another role is
    // malformed: the start refuses it before OpenVMM starts, and deprovision, which reads no
    // layers, still removes the sandbox.
    let root = Root::new();
    let output = run(&root, &provision(&root, "other-role", ""));
    assert!(output.status.success(), "{}", text(&output.stderr));
    let config = root.path.join("other-role").join("config.json");
    let mut document: Value = serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
    document["layers"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "path": root.path.join("runtime.erofs").to_str().unwrap(),
            "role": "runtime",
            "uuid": "22222222-2222-2222-2222-222222222222",
        }));
    fs::write(&config, serde_json::to_string_pretty(&document).unwrap()).unwrap();
    let output = run(&root, &lifecycle("start", &root, "other-role"));
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stderr),
        "error[io]: sandbox configuration is malformed\n"
    );
    assert!(!root.path.join("other-role").join("runtime.json").exists());
    let output = run(&root, &lifecycle("deprovision", &root, "other-role"));
    assert!(output.status.success(), "{}", text(&output.stderr));
}

/// What a start that fails the check of its distro binding adds to the check's message.
const PROVISION_AGAIN: &str = "; deprovision the sandbox and provision it again";

/// The SHA-256 digest of the fixture's distro layer.
const DISTRO_SHA256: &str = "8a475efd5fd711d481a9a19de6d12b88dff0cd81cb8135ffcd5b65b062c7bf04";

/// Returns a manifest of the fixture's distro layer that names it `artifact`, with its digest,
/// and lists `packages`.
fn distro_manifest(artifact: &str, packages: &[&str]) -> String {
    let packages: Vec<Value> = packages.iter().map(|name| json!({"name": name})).collect();
    json!({
        "format": 1,
        "artifact": artifact,
        "artifact_sha256": DISTRO_SHA256,
        "packages": packages,
    })
    .to_string()
}

/// Provisions a minimal sandbox in `state` below the root.
fn provisioned(root: &Root, state: &str) {
    provisioned_with(root, state, "");
}

/// Starts, stops, and deprovisions the provisioned sandbox of `state`.
fn cycle(root: &Root, state: &str) {
    for operation in ["start", "stop", "deprovision"] {
        let output = run(root, &lifecycle(operation, root, state));
        assert!(
            output.status.success(),
            "{operation}: {}",
            text(&output.stderr)
        );
    }
}

/// Starts the sandbox of `state`, which must fail with `code` and `message` before it launches
/// OpenVMM or writes any file, and deprovisions it.
fn refused_start(root: &Root, state: &str, code: &str, message: &str) {
    let output = run(root, &lifecycle("start", root, state));
    assert_eq!(output.status.code(), Some(1), "{}", text(&output.stderr));
    assert_eq!(text(&output.stderr), error_line(code, message));
    assert_eq!(state_entries(root, state), ["config.json", "sandbox.lock"]);
    let output = run(root, &lifecycle("deprovision", root, state));
    assert!(output.status.success(), "{}", text(&output.stderr));
}

/// Returns the standard error of a command that fails with the error `code` and `message`.
fn error_line(code: &str, message: &str) -> String {
    format!("error[{code}]: {message}\n")
}

#[test]
fn start_refuses_a_distro_layer_that_changed_after_provision() {
    // Start checks the binding that provision recorded against the layer's metadata alone, before
    // it launches OpenVMM; a layer that changed needs a new provision, which verifies it again.
    let root = Root::new();
    let distro = root.path.join("distro.erofs");
    let shown = distro.to_str().unwrap();
    provisioned(&root, "appended");
    let mut data = fs::read(&distro).unwrap();
    data.extend_from_slice(b"more\n");
    fs::write(&distro, &data).unwrap();
    refused_start(
        &root,
        "appended",
        "unavailable",
        &format!(
            "{shown} changed after its distro binding was made: its length differs{PROVISION_AGAIN}"
        ),
    );
    provisioned(&root, "replaced");
    let replacement = root.path.join("distro.new");
    fs::write(&replacement, &data).unwrap();
    fs::rename(&replacement, &distro).unwrap();
    refused_start(
        &root,
        "replaced",
        "unavailable",
        &format!(
            "{shown} changed after its distro binding was made: its identity differs{PROVISION_AGAIN}"
        ),
    );
    // The launch's own check of its layers still finds a missing layer first, as it did before.
    provisioned(&root, "removed");
    fs::remove_file(&distro).unwrap();
    refused_start(
        &root,
        "removed",
        "unavailable",
        &format!("distro EROFS layer not found: {shown}"),
    );
    // A new provision binds the layer as it is now.
    fs::write(&distro, &data).unwrap();
    provisioned(&root, "again");
    cycle(&root, "again");
}

#[test]
fn start_refuses_a_distro_manifest_that_changed_after_provision() {
    let root = Root::new();
    let distro = root.path.join("distro.erofs");
    let distro = distro.to_str().unwrap();
    let manifest = root.path.join("distro.erofs.manifest.json");
    let shown = manifest.to_str().unwrap();
    // Start never reads a manifest: one that appeared after the provision fails unread.
    provisioned(&root, "appeared");
    fs::write(&manifest, "not JSON").unwrap();
    refused_start(
        &root,
        "appeared",
        "unavailable",
        &format!("{shown} appeared after the distro binding of {distro} was made{PROVISION_AGAIN}"),
    );
    // Provision verifies a manifest that sits beside the layer, and binds it.
    let valid = distro_manifest("distro.erofs", &["busybox"]);
    fs::write(&manifest, &valid).unwrap();
    provisioned(&root, "intact");
    cycle(&root, "intact");
    provisioned(&root, "replaced");
    let replacement = root.path.join("manifest.new");
    fs::write(&replacement, &valid).unwrap();
    fs::rename(&replacement, &manifest).unwrap();
    refused_start(
        &root,
        "replaced",
        "unavailable",
        &format!(
            "{shown} changed after the distro binding of {distro} was made: its identity \
             differs{PROVISION_AGAIN}"
        ),
    );
    provisioned(&root, "removed");
    fs::remove_file(&manifest).unwrap();
    refused_start(
        &root,
        "removed",
        "unavailable",
        &format!(
            "{shown} disappeared after the distro binding of {distro} was made{PROVISION_AGAIN}"
        ),
    );
}

/// Makes `link` a symbolic link to the file `target`, and returns whether the host allowed it:
/// Windows lets only Developer Mode or a privilege create one.
fn symlink_file(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_file(target, link);
    linked.is_ok()
}

#[test]
fn a_distro_manifest_path_whose_lookup_fails_refuses_provision_and_start() {
    // Verification counts a manifest path whose lookup fails as no manifest, as nvx.py did, so a
    // run's outcome is unchanged. A binding of the manifest's absence holds only while nothing is
    // at its path: start refuses such a path, and provision refuses to record a binding that
    // every start would refuse.
    let root = Root::new();
    let manifest = root.path.join("distro.erofs.manifest.json");
    provisioned(&root, "absent");
    if !symlink_file(&manifest, &manifest) {
        eprintln!("skipped: this host does not let the test create a symbolic link");
        return;
    }
    let failed = |output: &Output| {
        let stderr = text(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(
            stderr.starts_with(&format!("error[io]: {manifest:?}: "))
                && stderr.contains("(os error "),
            "{stderr}"
        );
    };
    failed(&run(&root, &lifecycle("start", &root, "absent")));
    assert_eq!(
        state_entries(&root, "absent"),
        ["config.json", "sandbox.lock"]
    );
    let output = run(&root, &lifecycle("deprovision", &root, "absent"));
    assert!(output.status.success(), "{}", text(&output.stderr));

    failed(&run(&root, &provision(&root, "present", "")));
    assert!(!root.path.join("present").exists());

    let path = |name: &str| root.path.join(name).to_str().unwrap().to_owned();
    let dry_run: Vec<String> = vec![
        "sandbox".into(),
        "run".into(),
        "--layer".into(),
        format!(
            "distro,{},11111111-1111-1111-1111-111111111111",
            path("distro.erofs")
        ),
        "--scratch".into(),
        path("scratch.ext4"),
        "--dry-run".into(),
    ];
    let output = run(&root, &dry_run);
    assert!(output.status.success(), "{}", text(&output.stderr));
}

#[test]
fn provision_binds_a_linked_distro_layer_where_start_checks_it() {
    // Provision records a layer at its resolved path, and start checks the binding there, with
    // the manifest beside that path: through a link to a layer of another name, the manifest
    // beside the link's target. Provision verifies the layer there as well as at the path given.
    let root = Root::new();
    let distro = root.path.join("distro.erofs");
    let alias = root.path.join("alias.erofs");
    if !symlink_file(&distro, &alias) {
        eprintln!("skipped: this host does not let the test create a symbolic link");
        return;
    }
    let target_manifest = root.path.join("distro.erofs.manifest.json");
    let alias_manifest = root.path.join("alias.erofs.manifest.json");
    let provision_alias = |state: &str| run(&root, &provision_layer(&root, state, &alias, ""));
    let recorded = |state: &str| -> Value {
        let path = root.path.join(state).join("config.json");
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    };
    let refused = |state: &str, message: &str| {
        let output = provision_alias(state);
        assert_eq!(output.status.code(), Some(1), "{}", text(&output.stderr));
        assert_eq!(text(&output.stderr), error_line("unsupported", message));
        assert!(!root.path.join(state).exists());
    };
    let systemd = "systemd images are unsupported by the sandbox security profile";

    // A manifest beside the target alone: provision verifies it, and start checks it.
    fs::write(
        &target_manifest,
        distro_manifest("distro.erofs", &["systemd"]),
    )
    .unwrap();
    refused("target-systemd", systemd);
    fs::write(
        &target_manifest,
        distro_manifest("distro.erofs", &["busybox"]),
    )
    .unwrap();
    let output = provision_alias("target");
    assert!(output.status.success(), "{}", text(&output.stderr));
    let config = recorded("target");
    assert_eq!(config["layers"][0]["path"], distro.to_str().unwrap());
    assert!(config["distro_binding"]["manifest"].is_object());
    cycle(&root, "target");

    // A manifest beside the link alone: provision verifies it, as it verifies the layer at the
    // path given, and binds the absence of one beside the target, which start checks.
    fs::remove_file(&target_manifest).unwrap();
    fs::write(
        &alias_manifest,
        distro_manifest("alias.erofs", &["systemd"]),
    )
    .unwrap();
    refused("alias-systemd", systemd);
    fs::write(
        &alias_manifest,
        distro_manifest("alias.erofs", &["busybox"]),
    )
    .unwrap();
    let output = provision_alias("alias");
    assert!(output.status.success(), "{}", text(&output.stderr));
    let config = recorded("alias");
    assert_eq!(config["layers"][0]["path"], distro.to_str().unwrap());
    assert!(config["distro_binding"]["manifest"].is_null());
    cycle(&root, "alias");

    // Manifests beside both: provision verifies both, and binds the target's.
    fs::write(
        &target_manifest,
        distro_manifest("distro.erofs", &["busybox"]),
    )
    .unwrap();
    let output = provision_alias("both");
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert!(recorded("both")["distro_binding"]["manifest"].is_object());
    cycle(&root, "both");
}

/// An edit of a recorded configuration.
type ConfigEdit = fn(&mut Value);

#[test]
fn start_refuses_a_configuration_without_a_valid_distro_binding() {
    // nvx.py recorded no binding: a sandbox that it provisioned is provisioned again rather than
    // migrated. A binding that does not hold refuses the start before OpenVMM starts.
    let root = Root::new();
    let distro = root.path.join("distro.erofs");
    let distro = distro.to_str().unwrap();
    let uuid = "11111111-1111-1111-1111-111111111111";
    let other = "22222222-2222-2222-2222-222222222222";
    let malformed = "sandbox configuration is malformed".to_owned();
    let binding =
        |detail: &str| format!("the distro binding of {distro} {detail}{PROVISION_AGAIN}");
    // A binding that an earlier build wrote, or none, is of a format that this build does not
    // read, a damaged one is malformed state, and one that no longer holds is unavailable.
    let cases: [(&str, ConfigEdit, &str, String); 7] = [
        (
            "missing",
            |config| {
                config.as_object_mut().unwrap().remove("distro_binding");
            },
            "format-unsupported",
            format!("sandbox configuration has no distro binding{PROVISION_AGAIN}"),
        ),
        (
            "incomplete",
            |config| {
                config["distro_binding"]
                    .as_object_mut()
                    .unwrap()
                    .remove("manifest");
            },
            "io",
            malformed.clone(),
        ),
        (
            "unknown",
            |config| config["distro_binding"]["origin"] = json!("nvx.py"),
            "io",
            malformed,
        ),
        (
            "version",
            |config| config["distro_binding"]["version"] = json!(2),
            "unavailable",
            binding("has version 2, which this build does not check"),
        ),
        (
            "identity",
            |config| {
                config["distro_binding"]["identity"] =
                    json!("22222222-2222-2222-2222-222222222222");
            },
            "unavailable",
            binding(&format!(
                "names the erofs layer with identity {other}, not {uuid}"
            )),
        ),
        (
            "revision",
            |config| config["distro_binding"]["policy"]["revision"] = json!(2),
            "unavailable",
            binding("holds a verdict of distro policy revision 2, not 1"),
        ),
        (
            "rejected",
            |config| config["distro_binding"]["policy"]["result"] = json!("rejected"),
            "unsupported",
            binding("holds a verdict that rejects the layer"),
        ),
    ];
    for (state, edit, code, message) in cases {
        provisioned(&root, state);
        let path = root.path.join(state).join("config.json");
        let mut config: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        edit(&mut config);
        fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
        refused_start(&root, state, code, &message);
    }
}

/// Runs the lifecycle `operation` on `state` below the root, with `extra` arguments, and returns
/// its exit status and standard error.
fn transition(root: &Root, operation: &str, state: &str, extra: &[&str]) -> (Option<i32>, String) {
    let mut arguments = lifecycle(operation, root, state);
    arguments.extend(extra.iter().map(|argument| (*argument).to_owned()));
    let output = run(root, &arguments);
    (output.status.code(), text(&output.stderr))
}

/// Returns the exit status and standard error of a transition that succeeds.
fn succeeded() -> (Option<i32>, String) {
    (Some(0), String::new())
}

/// Returns the exit status and standard error of a transition that fails with the error `code`
/// and `message`.
fn failed(code: &str, message: &str) -> (Option<i32>, String) {
    (Some(1), error_line(code, message))
}

/// Provisions a minimal sandbox in `state` below the root, with `command_line` for the fake
/// OpenVMM.
fn provisioned_with(root: &Root, state: &str, command_line: &str) {
    let output = run(root, &provision(root, state, command_line));
    assert!(output.status.success(), "{}", text(&output.stderr));
}

/// Returns the runtime record of the sandbox in `state` below the root, if it has one.
fn runtime_record(root: &Root, state: &str) -> Option<Value> {
    let text = fs::read_to_string(root.path.join(state).join("runtime.json")).ok()?;
    Some(serde_json::from_str(&text).unwrap())
}

/// Returns the OpenVMM process that the runtime record of `state` names, while it runs.
fn recorded_process(root: &Root, state: &str) -> Option<Process> {
    let record = runtime_record(root, state)?;
    let reference: ProcessRef = serde_json::from_value(record["process"].clone()).unwrap();
    Process::open(&reference).unwrap()
}

/// Returns the names in the state directory `state` below the root, sorted.
fn state_entries(root: &Root, state: &str) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(root.path.join(state))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// The error of exec and stop for a sandbox whose OpenVMM exited without a stop.
const STALE: &str = "sandbox runtime state is stale because the OpenVMM process is not running";

#[test]
fn a_restarted_cli_drives_only_a_vm_of_its_runtime_identity() {
    // Each run of the CLI is a new orchestrator, which finds the VM that an earlier one started
    // within the same boot through the runtime record of the store.
    let root = Root::new();
    provisioned(&root, "state");
    assert_eq!(transition(&root, "start", "state", &[]), succeeded());
    let record = runtime_record(&root, "state").unwrap();
    // The record names the artifacts by the digests that their build provenance records.
    for (part, name, field) in [
        ("openvmm", "openvmm", "executable_sha256"),
        ("kernel", "vmlinux", "kernel_sha256"),
        ("initramfs", "initramfs", "initramfs_sha256"),
    ] {
        let path = root.below(&format!("build/{name}.provenance.json"));
        let provenance: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(
            record["runtime"][part].as_str().unwrap(),
            format!("sha256:{}", provenance[field].as_str().unwrap())
        );
    }
    let mut exec = lifecycle("exec", &root, "state");
    exec.extend(["--entrypoint", "/bin/echo", "--arg", "reattached"].map(str::to_owned));
    let output = run(&root, &exec);
    assert_eq!(
        (output.status.code(), text(&output.stdout)),
        (Some(0), "reattached\n".to_owned()),
        "{}",
        text(&output.stderr)
    );

    // Artifacts of another kernel did not start the VM, so no transition drives it, and it runs
    // on until a stop terminates it.
    let kernel = root.below("build/vmlinux.provenance.json");
    let provenance = fs::read_to_string(&kernel).unwrap();
    fs::write(&kernel, other_kernel_provenance()).unwrap();
    let pid = record["process"]["pid"].as_u64().unwrap();
    let mismatched = format!(
        "sandbox runs under another runtime identity than these artifacts, which differ in \
         kernel, so it is not driven: stop it to terminate its OpenVMM process {pid} without a \
         guest shutdown"
    );
    for operation in ["start", "exec"] {
        assert_eq!(
            transition(&root, operation, "state", &[]),
            failed("runtime-mismatch", &mismatched),
            "{operation}"
        );
    }
    assert_eq!(
        transition(&root, "deprovision", "state", &[]),
        failed(
            "sandbox-running",
            "sandbox must be stopped before deprovision"
        )
    );
    assert_eq!(runtime_record(&root, "state").unwrap(), record);
    let process = recorded_process(&root, "state").expect("the VM runs");

    // Its stop terminates OpenVMM through its verified process identity, and says so. It opens no
    // control session, whose protocol the guest might not speak.
    let sessions = sessions(&root, "state");
    assert!(sessions > 0, "the fake OpenVMM recorded no session");
    assert_eq!(
        transition(&root, "stop", "state", &[]),
        (Some(0), terminated("kernel", pid))
    );
    assert!(process.has_exited().unwrap());
    assert_eq!(self::sessions(&root, "state"), sessions);
    // The terminated OpenVMM published no outcome report, and its log stays.
    assert_eq!(
        state_entries(&root, "state"),
        ["config.json", "openvmm.log", "sandbox.lock"]
    );
    assert_eq!(
        transition(&root, "stop", "state", &[]),
        failed("sandbox-not-running", "sandbox is not running")
    );

    // The sandbox starts again, and stops gracefully under the identity that started it.
    fs::write(&kernel, provenance).unwrap();
    assert_eq!(transition(&root, "start", "state", &[]), succeeded());
    assert_eq!(transition(&root, "stop", "state", &[]), succeeded());
    assert_eq!(transition(&root, "deprovision", "state", &[]), succeeded());
}

/// Returns build provenance that names another kernel than the fixture's.
fn other_kernel_provenance() -> String {
    format!(
        "{{\"format\": 1, \"kernel_sha256\": \"{}\"}}\n",
        "0".repeat(64)
    )
}

/// Returns the standard error of a stop that terminated OpenVMM process `pid`, which ran under
/// another runtime identity than the artifacts, one that differs in `parts`.
fn terminated(parts: &str, pid: u64) -> String {
    format!(
        "warning: sandbox ran under another runtime identity than these artifacts, which differ \
         in {parts}: its OpenVMM process {pid} was terminated without a guest shutdown\n"
    )
}

/// Returns how many connections to its control endpoint the fake OpenVMM of the sandbox in
/// `state` below the root has accepted.
fn sessions(root: &Root, state: &str) -> usize {
    fs::read_to_string(root.below(&format!("build/fake-openvmm-{state}-sessions.log")))
        .map_or(0, |log| log.lines().count())
}

#[test]
fn a_stop_that_cannot_identify_the_runtime_keeps_the_vm_for_the_next_one() {
    let root = Root::new();
    provisioned(&root, "state");
    assert_eq!(transition(&root, "start", "state", &[]), succeeded());
    let record = runtime_record(&root, "state").unwrap();
    let pid = record["process"]["pid"].as_u64().unwrap();
    let process = recorded_process(&root, "state").expect("the VM runs");
    let sessions = sessions(&root, "state");

    // Without the install metadata of the initramfs, the runtime identity is indeterminate, and so
    // is whether the VM may be driven: the stop signals nothing, and keeps its record.
    let initramfs = root.below("build/initramfs.provenance.json");
    fs::remove_file(&initramfs).unwrap();
    assert_eq!(
        transition(&root, "stop", "state", &[]),
        failed(
            "unavailable",
            &format!(
                "the initramfs build provenance {} is missing, so the runtime cannot be \
                 identified: build the artifact with nvx.py, or install a release with nvx-rs \
                 download",
                initramfs.display()
            )
        )
    );
    assert_eq!(runtime_record(&root, "state").unwrap(), record);
    assert!(!process.has_exited().unwrap());

    // The next stop, once the metadata names another initramfs, terminates the VM.
    fs::write(
        &initramfs,
        format!(
            "{{\"format\": 1, \"initramfs_sha256\": \"{}\"}}\n",
            "0".repeat(64)
        ),
    )
    .unwrap();
    assert_eq!(
        transition(&root, "stop", "state", &[]),
        (Some(0), terminated("initramfs", pid))
    );
    assert!(process.has_exited().unwrap());
    assert_eq!(self::sessions(&root, "state"), sessions);
    assert_eq!(runtime_record(&root, "state"), None);
    assert_eq!(transition(&root, "deprovision", "state", &[]), succeeded());
}

#[test]
fn a_stop_under_another_identity_never_ends_a_process_that_its_record_does_not_name() {
    let root = Root::new();
    provisioned(&root, "state");
    let kernel = root.below("build/vmlinux.provenance.json");
    let provenance = fs::read_to_string(&kernel).unwrap();
    let path = root.path.join("state").join("runtime.json");
    // The record names the VMM's ID with another start time, as after the VMM exited and another
    // process reused the ID, or names another boot, every process of which has exited.
    for field in ["startTime", "boot"] {
        fs::write(&kernel, &provenance).unwrap();
        assert_eq!(transition(&root, "start", "state", &[]), succeeded());
        let process = recorded_process(&root, "state").expect("the VM runs");
        let mut record = runtime_record(&root, "state").unwrap();
        record["process"][field] = match &record["process"][field] {
            Value::Number(start_time) => json!(start_time.as_u64().unwrap() + 1),
            Value::String(boot) => {
                let flipped = if boot.starts_with('0') { "1" } else { "0" };
                json!(format!("{flipped}{}", &boot[1..]))
            }
            other => panic!("unexpected {field}: {other}"),
        };
        fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        // Under another runtime identity, the record names a VMM that exited, so the stop clears
        // it, and signals nothing.
        fs::write(&kernel, other_kernel_provenance()).unwrap();
        assert_eq!(
            transition(&root, "stop", "state", &[]),
            failed("sandbox-exited", STALE),
            "{field}"
        );
        assert_eq!(runtime_record(&root, "state"), None);
        assert!(
            !process
                .wait(Instant::now() + Duration::from_millis(200))
                .unwrap(),
            "{field}"
        );
        process.kill().unwrap();
        assert!(
            process
                .wait(Instant::now() + Duration::from_secs(10))
                .unwrap()
        );
    }
    assert_eq!(transition(&root, "deprovision", "state", &[]), succeeded());
}

#[test]
fn a_start_that_was_interrupted_is_ended_under_another_identity_too() {
    let root = Root::new();
    // A guest that answers after a minute keeps the start waiting.
    provisioned_with(&root, "state", "fake_boot_delay_ms=60000");
    let mut start = Command::new(NVX_RS)
        .arg("--root")
        .arg(&root.path)
        .args(lifecycle("start", &root, "state"))
        .current_dir(&root.path)
        .env_remove("NVX_OPENVMM")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // The start recorded its OpenVMM in its launch journal right after it spawned it.
    let journal = root.path.join("state").join("launch.json");
    let deadline = Instant::now() + Duration::from_secs(30);
    let reference: ProcessRef = loop {
        let recorded = fs::read_to_string(&journal)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|journal| serde_json::from_value(journal.get("process")?.clone()).ok());
        if let Some(reference) = recorded {
            break reference;
        }
        assert!(Instant::now() < deadline, "the start recorded no OpenVMM");
        thread::sleep(Duration::from_millis(20));
    };
    // The CLI dies, as a crash would, and leaves its OpenVMM running.
    start.kill().unwrap();
    start.wait().unwrap();
    let process = Process::open(&reference)
        .unwrap()
        .expect("the OpenVMM of the start runs");

    // Under another runtime identity, the next transition ends the start, whose OpenVMM it
    // verifies through the journal.
    fs::write(
        root.below("build/vmlinux.provenance.json"),
        other_kernel_provenance(),
    )
    .unwrap();
    assert_eq!(
        transition(&root, "stop", "state", &[]),
        failed("sandbox-not-running", "sandbox is not running")
    );
    assert!(process.has_exited().unwrap());
    assert_eq!(
        state_entries(&root, "state"),
        ["config.json", "openvmm.log", "sandbox.lock"]
    );
    assert_eq!(transition(&root, "deprovision", "state", &[]), succeeded());
}

#[test]
fn runtime_records_of_earlier_formats_fail_and_name_their_format() {
    let root = Root::new();
    provisioned(&root, "state");
    // The runtime record that the CLI wrote before the store, in its format 1.
    let path = root.path.join("state").join("runtime.json");
    let endpoint = root.path.join("state").join("control.sock");
    let record = format!(
        "{{\n  \"control_endpoint\": {},\n  \"format\": 1,\n  \"pid\": 4242,\n  \
         \"start_time\": 1\n}}\n",
        serde_json::to_string(endpoint.to_str().unwrap()).unwrap()
    );
    fs::write(&path, &record).unwrap();
    let unsupported = format!(
        "the sandbox runtime record {} has the unsupported format 1; this build reads format 4 \
         only and migrates no other, so stop the sandbox with the build that started it",
        path.display()
    );
    for operation in ["start", "exec", "stop", "deprovision"] {
        assert_eq!(
            transition(&root, operation, "state", &[]),
            failed("format-unsupported", &unsupported),
            "{operation}"
        );
    }
    assert_eq!(fs::read_to_string(&path).unwrap(), record);
    fs::remove_file(&path).unwrap();
    assert_eq!(transition(&root, "deprovision", "state", &[]), succeeded());
}

#[test]
fn a_managed_start_runs_only_the_openvmm_of_the_root() {
    // The install metadata of the root identifies its own OpenVMM alone, so a start refuses
    // another one, which the runtime record would misname.
    let root = Root::new();
    provisioned(&root, "state");
    let environment = BTreeMap::from([("NVX_OPENVMM".to_owned(), FAKE_OPENVMM.to_owned())]);
    let output = nvx_rs(&root, &lifecycle("start", &root, "state"), &environment);
    assert_eq!(
        (output.status.code(), text(&output.stderr)),
        failed(
            "unsupported",
            "a managed sandbox runs the OpenVMM of the artifact root, which its install metadata \
             identifies, and not the one that NVX_OPENVMM names"
        )
    );
    assert_eq!(
        state_entries(&root, "state"),
        ["config.json", "sandbox.lock"]
    );
    // The transitions that launch nothing do not run the OpenVMM that it names.
    assert_eq!(transition(&root, "start", "state", &[]), succeeded());
    let output = nvx_rs(&root, &lifecycle("stop", &root, "state"), &environment);
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(transition(&root, "deprovision", "state", &[]), succeeded());
}

#[test]
fn stop_reports_its_timeout_and_leaves_the_vm_running() {
    let root = Root::new();
    // A guest that never answers the stop.
    provisioned_with(&root, "unanswered", "fake_ignore_stop=1");
    assert_eq!(transition(&root, "start", "unanswered", &[]), succeeded());
    let record = runtime_record(&root, "unanswered").unwrap();
    assert_eq!(
        transition(&root, "stop", "unanswered", &["--timeout", "1"]),
        failed("timed-out", "managed control response timed out")
    );
    assert_eq!(runtime_record(&root, "unanswered").unwrap(), record);
    let process = recorded_process(&root, "unanswered").expect("the VM still runs");
    process.kill().unwrap();
    assert!(
        process
            .wait(Instant::now() + Duration::from_secs(10))
            .unwrap()
    );
    assert_eq!(
        transition(&root, "deprovision", "unanswered", &[]),
        succeeded()
    );

    // A guest that shuts down, whose OpenVMM takes longer to exit than the stop waits.
    provisioned_with(&root, "slow", "fake_exit_delay_ms=4000");
    assert_eq!(transition(&root, "start", "slow", &[]), succeeded());
    let record = runtime_record(&root, "slow").unwrap();
    assert_eq!(
        transition(&root, "stop", "slow", &["--timeout", "1"]),
        failed("timed-out", "OpenVMM did not terminate after managed stop")
    );
    assert_eq!(runtime_record(&root, "slow").unwrap(), record);
    let process = recorded_process(&root, "slow").expect("the VM still runs");
    assert!(
        process
            .wait(Instant::now() + Duration::from_secs(30))
            .unwrap()
    );
    assert_eq!(
        transition(&root, "stop", "slow", &[]),
        failed("sandbox-exited", STALE)
    );
    assert_eq!(runtime_record(&root, "slow"), None);
    assert_eq!(transition(&root, "deprovision", "slow", &[]), succeeded());
}

#[test]
fn a_sandbox_whose_openvmm_exited_reports_it_once_and_starts_again() {
    let root = Root::new();
    provisioned(&root, "state");
    let crash = || {
        assert_eq!(transition(&root, "start", "state", &[]), succeeded());
        let process = recorded_process(&root, "state").expect("the VM runs");
        process.kill().unwrap();
        assert!(
            process
                .wait(Instant::now() + Duration::from_secs(10))
                .unwrap()
        );
    };
    crash();
    assert_eq!(
        transition(&root, "exec", "state", &[]),
        failed("sandbox-exited", STALE)
    );
    assert_eq!(
        state_entries(&root, "state"),
        ["config.json", "openvmm.log", "sandbox.lock"]
    );
    assert_eq!(
        transition(&root, "stop", "state", &[]),
        failed("sandbox-not-running", "sandbox is not running")
    );
    // A start replaces the record of an OpenVMM that exited.
    crash();
    assert_eq!(transition(&root, "start", "state", &[]), succeeded());
    assert_eq!(transition(&root, "stop", "state", &[]), succeeded());
    assert_eq!(transition(&root, "deprovision", "state", &[]), succeeded());
}

/// The network options, after `--net 10.0.0.2/24 --network-profile portable` unless they start
/// with `-`, that the planner refuses before anything launches, and its messages.
const REFUSED_NETWORKS: [(&[&str], &str); 27] = [
    // A proxy is the guest's only way out.
    (
        &["--network-proxy", "10.0.0.1:3128"],
        "a network proxy needs egress denied by default, since the proxy is the guest's only way \
         out",
    ),
    (
        &[
            "--network-egress",
            "deny",
            "--network-proxy",
            "10.0.0.1:3128",
        ],
        "a network proxy needs host-loopback access denied",
    ),
    (
        &[
            "--network-egress",
            "deny",
            "--host-loopback",
            "allow",
            "--network-proxy",
            "10.0.0.1:3128",
        ],
        "a network proxy needs host-loopback access denied",
    ),
    (
        &[
            "--network-egress",
            "deny",
            "--network-ingress",
            "allow",
            "--host-loopback",
            "deny",
            "--network-proxy",
            "10.0.0.1:3128",
        ],
        "a network proxy needs ingress denied",
    ),
    (
        &[
            "--network-egress",
            "deny",
            "--network-egress-allow",
            "192.0.2.0/24:tcp:443",
            "--host-loopback",
            "deny",
            "--network-proxy",
            "10.0.0.1:3128",
        ],
        "a network proxy takes no --network-egress-allow rules, since the proxy is the guest's \
         only way out",
    ),
    (
        &[
            "--network-egress",
            "deny",
            "--network-egress-deny",
            "192.0.2.0/24",
            "--host-loopback",
            "deny",
            "--network-proxy",
            "10.0.0.1:3128",
        ],
        "a network proxy takes no --network-egress-deny rules, since the proxy is the guest's only \
         way out",
    ),
    (
        &[
            "--network-egress",
            "deny",
            "--host-loopback",
            "deny",
            "--network-proxy",
            "10.0.0.1:3128",
            "--host-loopback-forward",
            "tcp:18080:8080",
        ],
        "a network proxy takes no host-loopback forwards",
    ),
    // The proxy is at the guest's gateway, on an explicit port.
    (
        &[
            "--network-egress",
            "deny",
            "--host-loopback",
            "deny",
            "--network-proxy",
            "198.51.100.1:3128",
        ],
        "--network-proxy 198.51.100.1:3128: the address must be the guest's gateway, 10.0.0.1",
    ),
    (
        &["--network-proxy", "10.0.0.1"],
        "--network-proxy 10.0.0.1: expected IPV4:TCP-PORT",
    ),
    (
        &["--network-proxy", "10.0.0.1:0"],
        "--network-proxy 10.0.0.1:0: the port must be between 1 and 65535",
    ),
    // OpenVMM answers DNS at the gateway's port 53 before it relays to a proxy.
    (
        &[
            "--network-egress",
            "deny",
            "--host-loopback",
            "deny",
            "--network-proxy",
            "10.0.0.1:53",
        ],
        "a network proxy needs a port other than 53, where OpenVMM answers DNS at the guest's \
         gateway",
    ),
    // Forwards need host-loopback access allowed, and UDP forwards egress for their replies.
    (
        &["--host-loopback-forward", "tcp:18080:8080"],
        "host-loopback forwards need host-loopback access allowed",
    ),
    (
        &[
            "--network-egress",
            "deny",
            "--host-loopback",
            "allow",
            "--host-loopback-forward",
            "udp:18080:8080",
        ],
        "host-loopback UDP forwards need egress allowed by default for their replies",
    ),
    (
        &[
            "--network-egress",
            "allow",
            "--network-egress-deny",
            "10.0.0.0/8",
            "--host-loopback",
            "allow",
            "--host-loopback-forward",
            "udp:18080:8080",
        ],
        "--network-egress-deny[0] cannot cover the guest network, which carries host-loopback UDP \
         forward replies",
    ),
    (
        &[
            "--host-loopback",
            "allow",
            "--host-loopback-forward",
            "8080",
        ],
        "--host-loopback-forward 8080: expected tcp:HOST_PORT:GUEST_PORT or \
         udp:HOST_PORT:GUEST_PORT",
    ),
    (
        &[
            "--host-loopback",
            "allow",
            "--host-loopback-forward",
            "tcp:18080:8080",
            "--host-loopback-forward",
            "tcp:18080:8081",
        ],
        "host-loopback forwards bind tcp host port 18080 more than once",
    ),
    // OpenVMM presents host peers at an address of the guest network.
    (
        &[
            "-",
            "--net",
            "10.0.0.2/30",
            "--network-profile",
            "portable",
            "--host-loopback",
            "allow",
        ],
        "host-loopback access needs a guest network prefix of /29 or shorter, which leaves an \
         address at which OpenVMM presents host peers to the guest",
    ),
    // The guest network and the rules are read as OpenVMM reads them.
    (
        &["-", "--net", "10.0.0.1/24", "--network-profile", "portable"],
        "guest network \"10.0.0.1/24\" must be an IPv4 address with a /1 to /30 prefix that is not \
         its network's network, broadcast, or gateway (first) address",
    ),
    (
        &["--network-egress-allow", "192.0.2.0/24"],
        "--network-egress is required with --network-egress-allow or --network-egress-deny",
    ),
    (
        &[
            "--network-egress",
            "deny",
            "--network-egress-allow",
            "192.0.2.0/33",
        ],
        "--network-egress-allow 192.0.2.0/33: the destination must be an IPv4 or IPv6 address \
         with an optional prefix",
    ),
    (
        &[
            "--network-egress",
            "deny",
            "--network-egress-allow",
            "192.0.2.0/24:sctp",
        ],
        "--network-egress-allow 192.0.2.0/24:sctp: the protocol must be tcp, udp, or icmp",
    ),
    (
        &[
            "--network-egress",
            "allow",
            "--network-egress-deny",
            "192.0.2.0/24:icmp:8",
        ],
        "--network-egress-deny 192.0.2.0/24:icmp:8: icmp takes no port",
    ),
    (
        &[
            "--network-egress",
            "deny",
            "--network-egress-allow",
            "192.0.2.0/24:tcp:90-80",
        ],
        "--network-egress-allow 192.0.2.0/24:tcp:90-80: the port range ends below its first port",
    ),
    // Without --net, no other network option applies.
    (
        &["-", "--network-egress", "deny"],
        "network policy options require --net",
    ),
    (
        &["-", "--host-loopback", "deny"],
        "network policy options require --net",
    ),
    (
        &["-", "--network-proxy", "10.0.0.1:3128"],
        "network policy options require --net",
    ),
    (
        &["-", "--host-loopback-forward", "tcp:18080:8080"],
        "network policy options require --net",
    ),
];

#[test]
fn network_policies_that_the_planner_refuses_fail_before_launch() {
    // The planner refuses what OpenVMM refused, and what a proxy-only network excludes, before a
    // run prints its command and before a provision records anything.
    let root = Root::new();
    let path = |name: &str| root.path.join(name).to_str().unwrap().to_owned();
    for (index, (options, message)) in REFUSED_NETWORKS.iter().enumerate() {
        let options: Vec<String> = match options.split_first() {
            Some((&"-", options)) => options.iter().map(|&option| option.to_owned()).collect(),
            _ => ["--net", "10.0.0.2/24", "--network-profile", "portable"]
                .iter()
                .chain(options.iter())
                .map(|&option| option.to_owned())
                .collect(),
        };
        let state = format!("refused-{index}");
        let commands = [
            vec!["run".to_owned(), "--dry-run".to_owned()],
            vec![
                "provision".to_owned(),
                "--state-dir".to_owned(),
                path(&state),
            ],
        ];
        for command in commands {
            let mut arguments = vec!["sandbox".to_owned()];
            arguments.extend(command);
            arguments.extend([
                "--layer".to_owned(),
                format!(
                    "distro,{},11111111-1111-1111-1111-111111111111",
                    path("distro.erofs")
                ),
                "--scratch".to_owned(),
                path("scratch.ext4"),
            ]);
            arguments.extend(options.iter().cloned());
            let output = run(&root, &arguments);
            assert_eq!(output.status.code(), Some(1), "{arguments:?}");
            assert_eq!(text(&output.stdout), "", "{arguments:?}");
            assert_eq!(
                text(&output.stderr),
                error_line("unsupported", message),
                "{arguments:?}"
            );
        }
        assert!(!root.path.join(&state).exists(), "{options:?}");
    }
}

#[test]
fn tcp_forwards_take_any_egress_policy() {
    // OpenVMM lets the guest's replies on a forwarded TCP connection through whatever the egress
    // policy, so the planner accepts TCP forwards under denied egress and beside a deny rule that
    // covers the guest network, as OpenVMM did.
    let root = Root::new();
    let path = |name: &str| root.path.join(name).to_str().unwrap().to_owned();
    for (egress, rendered) in [
        (
            &["--network-egress", "deny"][..],
            " --net 10.0.0.2/24 --network-profile portable --network-egress deny \
             --network-ingress deny --host-loopback allow --host-loopback-forward tcp:18080:8080 \
             --microvm-lifecycle ",
        ),
        (
            &[
                "--network-egress",
                "allow",
                "--network-egress-deny",
                "10.0.0.0/8",
            ][..],
            " --net 10.0.0.2/24 --network-profile portable --network-egress allow \
             --network-ingress deny --host-loopback allow --network-egress-deny 10.0.0.0/8 \
             --host-loopback-forward tcp:18080:8080 --microvm-lifecycle ",
        ),
    ] {
        let mut arguments: Vec<String> = [
            "sandbox",
            "run",
            "--dry-run",
            "--layer",
            &format!(
                "distro,{},11111111-1111-1111-1111-111111111111",
                path("distro.erofs")
            ),
            "--scratch",
            &path("scratch.ext4"),
            "--net",
            "10.0.0.2/24",
            "--network-profile",
            "portable",
        ]
        .map(str::to_owned)
        .to_vec();
        arguments.extend(egress.iter().map(|&option| option.to_owned()));
        arguments.extend(
            [
                "--host-loopback",
                "allow",
                "--host-loopback-forward",
                "tcp:18080:8080",
            ]
            .map(str::to_owned),
        );
        let output = run(&root, &arguments);
        assert!(output.status.success(), "{}", text(&output.stderr));
        assert!(
            text(&output.stdout).contains(rendered),
            "{}",
            text(&output.stdout)
        );
    }
}

#[test]
fn network_policy_files_compile_into_the_typed_policy() {
    // The compiled rules join the typed policy as explicit rules do, so a proxy refuses them too.
    let root = Root::new();
    let policy = root.path.join("policy.json");
    fs::write(&policy, r#"{"allow": [{"cidr": "192.0.2.0/24"}]}"#).unwrap();
    let path = |name: &str| root.path.join(name).to_str().unwrap().to_owned();
    let arguments: Vec<String> = [
        "sandbox",
        "run",
        "--dry-run",
        "--layer",
        &format!(
            "distro,{},11111111-1111-1111-1111-111111111111",
            path("distro.erofs")
        ),
        "--scratch",
        &path("scratch.ext4"),
        "--net",
        "10.0.0.2/24",
        "--network-profile",
        "portable",
        "--network-egress",
        "deny",
        "--network-egress-policy-file",
        policy.to_str().unwrap(),
        "--host-loopback",
        "deny",
        "--network-proxy",
        "10.0.0.1:3128",
    ]
    .map(str::to_owned)
    .to_vec();
    let output = run(&root, &arguments);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stderr),
        "error[unsupported]: a network proxy takes no --network-egress-allow rules, since the \
         proxy is the guest's only way out\n"
    );
    // Without the proxy, the compiled rules render as the policy file compiles them.
    let output = run(&root, &arguments[..arguments.len() - 4]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert!(
        text(&output.stdout).contains(
            " --net 10.0.0.2/24 --network-profile portable --network-egress deny \
             --network-ingress deny --network-egress-allow 192.0.2.0/24 --microvm-lifecycle "
        ),
        "{}",
        text(&output.stdout)
    );
}

/// The serde form of a valid proxy-only network, as a configuration records it.
fn recorded_proxy_only_network() -> Value {
    json!({
        "guestNetwork": "10.0.0.2/24",
        "egress": "deny",
        "allow": {"name": "--network-egress-allow", "rules": []},
        "deny": {"name": "--network-egress-deny", "rules": []},
        "ingress": "deny",
        "hostLoopback": "deny",
        "hostLoopbackForwards": [],
        "proxy": {"port": 3128, "scheme": null},
    })
}

#[test]
fn start_refuses_a_configuration_without_a_valid_network_policy() {
    // A configuration records the typed network that provision checked, which start reads and
    // checks again; no reader reads the network options that nvx.py and earlier releases
    // recorded.
    let root = Root::new();
    let malformed = "sandbox configuration is malformed".to_owned();
    let cases: [(&str, ConfigEdit, &str, String); 5] = [
        (
            "missing",
            |config| {
                config.as_object_mut().unwrap().remove("network");
            },
            "format-unsupported",
            format!("sandbox configuration has no network policy{PROVISION_AGAIN}"),
        ),
        (
            "options",
            |config| {
                let config = config.as_object_mut().unwrap();
                config.remove("network");
                config.insert("net".to_owned(), json!("10.0.0.2/24"));
                config.insert("network_profile".to_owned(), json!("portable"));
            },
            "format-unsupported",
            format!("sandbox configuration has no network policy{PROVISION_AGAIN}"),
        ),
        (
            "text",
            |config| config["network"] = json!("10.0.0.2/24"),
            "io",
            malformed.clone(),
        ),
        (
            "unknown",
            |config| {
                config["network"] = recorded_proxy_only_network();
                config["network"]["dns"] = json!("10.0.0.1");
            },
            "io",
            malformed.clone(),
        ),
        (
            "refused",
            |config| {
                config["network"] = recorded_proxy_only_network();
                config["network"]["egress"] = json!("allow");
            },
            "io",
            malformed,
        ),
    ];
    for (state, edit, code, message) in cases {
        provisioned(&root, state);
        let path = root.path.join(state).join("config.json");
        let mut config: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(config["network"], Value::Null);
        edit(&mut config);
        fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
        refused_start(&root, state, code, &message);
    }
}

#[test]
fn start_refuses_a_scratch_image_that_changed_after_provision() {
    // Start checks the binding of the scratch image that provision recorded against the image's
    // metadata alone, before it launches OpenVMM: an image that changed needs a new provision.
    let root = Root::new();
    let scratch = root.path.join("scratch.ext4");
    let shown = scratch.to_str().unwrap();
    let changed = |field: &str| {
        format!(
            "{shown} changed after its scratch binding was made: its {field} differs\
             {PROVISION_AGAIN}"
        )
    };
    // Another length changes the guest's block device.
    provisioned(&root, "resized");
    let mut data = fs::read(&scratch).unwrap();
    data.extend_from_slice(b"more\n");
    fs::write(&scratch, &data).unwrap();
    refused_start(&root, "resized", "unavailable", &changed("length"));
    // Another file in its place, even with the same content, has another identity.
    provisioned(&root, "replaced");
    let replacement = root.path.join("scratch.new");
    fs::write(&replacement, &data).unwrap();
    fs::rename(&replacement, &scratch).unwrap();
    refused_start(&root, "replaced", "unavailable", &changed("identity"));
    // The launch's own check of its images still finds a missing image first.
    provisioned(&root, "removed");
    fs::remove_file(&scratch).unwrap();
    refused_start(
        &root,
        "removed",
        "unavailable",
        &format!("ext4 scratch image not found: {shown}"),
    );
    // A new provision binds the image as it is now. Writes of the same length, as OpenVMM's,
    // keep the binding.
    fs::write(&scratch, &data).unwrap();
    provisioned(&root, "again");
    data[0] = b'S';
    fs::write(&scratch, &data).unwrap();
    cycle(&root, "again");
}

#[test]
fn start_refuses_a_configuration_without_a_valid_scratch_binding() {
    let root = Root::new();
    let scratch = root.path.join("scratch.ext4");
    let scratch = scratch.to_str().unwrap();
    let malformed = "sandbox configuration is malformed".to_owned();
    let changed = |field: &str| {
        format!(
            "{scratch} changed after its scratch binding was made: its {field} differs\
             {PROVISION_AGAIN}"
        )
    };
    let cases: [(&str, ConfigEdit, &str, String); 6] = [
        (
            "missing",
            |config| {
                config.as_object_mut().unwrap().remove("scratch_binding");
            },
            "format-unsupported",
            format!("sandbox configuration has no scratch binding{PROVISION_AGAIN}"),
        ),
        (
            "incomplete",
            |config| {
                config["scratch_binding"]
                    .as_object_mut()
                    .unwrap()
                    .remove("length");
            },
            "io",
            malformed.clone(),
        ),
        (
            "unknown",
            |config| config["scratch_binding"]["owner"] = json!("another"),
            "io",
            malformed,
        ),
        (
            "version",
            |config| config["scratch_binding"]["version"] = json!(2),
            "unavailable",
            format!(
                "the scratch binding of {scratch} has version 2, which this build does not \
                 check{PROVISION_AGAIN}"
            ),
        ),
        (
            "kind",
            |config| config["scratch_binding"]["kind"] = json!("directory"),
            "unavailable",
            changed("kind"),
        ),
        (
            "scheme",
            |config| {
                config["scratch_binding"]["scheme"] =
                    json!(if cfg!(windows) { "linux" } else { "windows" });
            },
            "unavailable",
            changed("seal scheme"),
        ),
    ];
    for (state, edit, code, message) in cases {
        provisioned(&root, state);
        let path = root.path.join(state).join("config.json");
        let mut config: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        edit(&mut config);
        fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
        refused_start(&root, state, code, &message);
    }
}

#[test]
fn a_running_sandbox_claims_its_scratch_image() {
    // Two sandboxes may record the same scratch image, but only one at a time runs on it: a start
    // claims the image until its OpenVMM exits, and a second start, or a run, fails meanwhile.
    let root = Root::new();
    let scratch = root.path.join("scratch.ext4");
    let claimed = error_line(
        "scratch-claimed",
        &format!(
            "another sandbox claims the ext4 scratch image {}",
            scratch.to_str().unwrap()
        ),
    );
    provisioned(&root, "first");
    provisioned(&root, "second");
    let output = run(&root, &lifecycle("start", &root, "first"));
    assert!(output.status.success(), "{}", text(&output.stderr));

    let output = run(&root, &lifecycle("start", &root, "second"));
    assert_eq!(output.status.code(), Some(1), "{}", text(&output.stderr));
    assert_eq!(text(&output.stderr), claimed);
    assert_eq!(
        state_entries(&root, "second"),
        ["config.json", "sandbox.lock"]
    );
    let one_shot: Vec<String> = vec![
        "sandbox".into(),
        "run".into(),
        "--layer".into(),
        format!(
            "distro,{},11111111-1111-1111-1111-111111111111",
            root.path.join("distro.erofs").to_str().unwrap()
        ),
        "--scratch".into(),
        scratch.to_str().unwrap().to_owned(),
        "--entrypoint".into(),
        "/bin/true".into(),
    ];
    let output = run(&root, &one_shot);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output.stderr));
    assert_eq!(text(&output.stderr), claimed);

    // Once the first sandbox stops, the image is free again.
    let output = run(&root, &lifecycle("stop", &root, "first"));
    assert!(output.status.success(), "{}", text(&output.stderr));
    cycle(&root, "second");
    let output = run(&root, &one_shot);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let output = run(&root, &lifecycle("deprovision", &root, "first"));
    assert!(output.status.success(), "{}", text(&output.stderr));
}

/// Waits until the file at `path` exists.
fn wait_for(path: &Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "{} did not appear",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// Waits until the file at `path` holds a whole JSON document, and returns it: the fake OpenVMM
/// writes its dumps with `fs::write`, which creates a file before it writes the file's content.
fn wait_for_json(path: &Path) -> Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let parsed = fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok());
        if let Some(document) = parsed {
            return document;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{} holds no whole JSON document",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn start_hands_openvmm_the_images_that_it_checked() {
    // Images that something replaces after the start checked them and before OpenVMM opens them:
    // on Linux, OpenVMM opens the checked files through the descriptors that it inherits, whatever
    // has their paths now; on Windows, the checked files keep anything from replacing the images
    // until the guest is ready.
    let root = Root::new();
    let output = run(
        &root,
        &provision(
            &root,
            "state",
            "fake_block_delay_ms=1500 fake_boot_delay_ms=1500",
        ),
    );
    assert!(output.status.success(), "{}", text(&output.stderr));
    let distro = root.path.join("distro.erofs");
    let scratch = root.path.join("scratch.ext4");
    #[cfg(windows)]
    let moved = |path: &Path| path.with_extension("moved");
    let start = Command::new(NVX_RS)
        .arg("--root")
        .arg(&root.path)
        .args(lifecycle("start", &root, "state"))
        .current_dir(&root.path)
        .env_remove("NVX_OPENVMM")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // OpenVMM has started, and has not opened its images yet.
    wait_for(&root.path.join("build").join("fake-openvmm-state.json"));
    let blocks = root
        .path
        .join("build")
        .join("fake-openvmm-state-blocks.json");
    assert!(!blocks.exists());
    for (path, replacement) in [
        (&distro, "replaced distro\n"),
        (&scratch, "replaced scratch\n"),
    ] {
        let new = path.with_extension("new");
        fs::write(&new, replacement).unwrap();
        let replaced = fs::rename(&new, path);
        assert_eq!(
            replaced.is_ok(),
            cfg!(unix),
            "{}: {replaced:?}",
            path.display()
        );
    }
    // On Windows, while the start holds the checked files, nothing writes the distro layer, and
    // nothing removes or renames either image, even once OpenVMM reads and writes the scratch
    // image.
    #[cfg(windows)]
    for opened in [false, true] {
        if opened {
            wait_for(&blocks);
        }
        assert!(fs::OpenOptions::new().append(true).open(&distro).is_err());
        for path in [&distro, &scratch] {
            assert!(fs::rename(path, moved(path)).is_err());
            assert!(fs::remove_file(path).is_err());
        }
    }

    let output = start.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", text(&output.stderr));
    // OpenVMM opened the objects that the start checked.
    let opened: Value = serde_json::from_str(&fs::read_to_string(&blocks).unwrap()).unwrap();
    let opened: Vec<(&str, &str)> = opened
        .as_array()
        .unwrap()
        .iter()
        .map(|block| {
            (
                block["role"].as_str().unwrap(),
                block["content"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(opened, [("distro", "distro\n"), ("scratch", "scratch\n")]);
    // Once the guest is ready, the start releases the checked files: on Windows, the images can be
    // renamed again, as OpenVMM's own opens share it.
    #[cfg(windows)]
    for path in [&distro, &scratch] {
        fs::rename(path, moved(path)).unwrap();
        fs::rename(moved(path), path).unwrap();
    }
    for operation in ["stop", "deprovision"] {
        let output = run(&root, &lifecycle(operation, &root, "state"));
        assert!(
            output.status.success(),
            "{operation}: {}",
            text(&output.stderr)
        );
    }
}

#[test]
fn a_start_that_ends_before_openvmm_opens_the_images_leaves_it_the_checked_ones() {
    // A start that ends after it spawned OpenVMM and before OpenVMM opened its images, as when the
    // CLI crashes, closes its checked files early: on Windows, the copies of them that OpenVMM
    // inherited keep anything from replacing the images at their final paths until OpenVMM exits;
    // on Linux, OpenVMM opens the checked files through the descriptors that it inherited,
    // whatever has their paths. The sandbox's next transition ends that OpenVMM.
    let root = Root::new();
    // OpenVMM opens its images seconds after it starts, and a guest that answers after a minute
    // keeps the start waiting.
    provisioned_with(
        &root,
        "state",
        "fake_block_delay_ms=3000 fake_boot_delay_ms=60000",
    );
    let distro = root.path.join("distro.erofs");
    let scratch = root.path.join("scratch.ext4");
    let mut start = Command::new(NVX_RS)
        .arg("--root")
        .arg(&root.path)
        .args(lifecycle("start", &root, "state"))
        .current_dir(&root.path)
        .env_remove("NVX_OPENVMM")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // The start recorded its OpenVMM in its launch journal right after it spawned it.
    let journal = root.path.join("state").join("launch.json");
    let deadline = Instant::now() + Duration::from_secs(30);
    let reference: ProcessRef = loop {
        let recorded = fs::read_to_string(&journal)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|journal| serde_json::from_value(journal.get("process")?.clone()).ok());
        if let Some(reference) = recorded {
            break reference;
        }
        assert!(Instant::now() < deadline, "the start recorded no OpenVMM");
        thread::sleep(Duration::from_millis(20));
    };
    // The CLI dies, as a crash would, before OpenVMM has opened its images, and leaves it running.
    start.kill().unwrap();
    start.wait().unwrap();
    let process = Process::open(&reference)
        .unwrap()
        .expect("the OpenVMM of the start runs");
    let blocks = root
        .path
        .join("build")
        .join("fake-openvmm-state-blocks.json");
    assert!(!blocks.exists());
    for (path, replacement) in [
        (&distro, "replaced distro\n"),
        (&scratch, "replaced scratch\n"),
    ] {
        let new = path.with_extension("new");
        fs::write(&new, replacement).unwrap();
        let replaced = fs::rename(&new, path);
        assert_eq!(
            replaced.is_ok(),
            cfg!(unix),
            "{}: {replaced:?}",
            path.display()
        );
    }

    // OpenVMM opens the objects that the start checked, and dumps what they held while the start's
    // OpenVMM still runs: the dump is whole once it parses, which no prefix of it does.
    let opened = wait_for_json(&blocks);
    let opened: Vec<(&str, &str)> = opened
        .as_array()
        .unwrap()
        .iter()
        .map(|block| {
            (
                block["role"].as_str().unwrap(),
                block["content"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(opened, [("distro", "distro\n"), ("scratch", "scratch\n")]);

    // The next transition ends the start, whose OpenVMM it verifies through the journal, and the
    // copies of the checked files go with OpenVMM: on Windows, the images can be renamed again.
    assert_eq!(
        transition(&root, "stop", "state", &[]),
        failed("sandbox-not-running", "sandbox is not running")
    );
    assert!(process.has_exited().unwrap());
    #[cfg(windows)]
    for path in [&distro, &scratch] {
        let moved = path.with_extension("moved");
        fs::rename(path, &moved).unwrap();
        fs::rename(&moved, path).unwrap();
    }
    assert_eq!(transition(&root, "deprovision", "state", &[]), succeeded());
}

/// Makes `handle` inheritable, or not, as code that spawns without a handle list does around its
/// spawns.
#[cfg(windows)]
fn set_inheritable(handle: &impl std::os::windows::io::AsRawHandle, inheritable: bool) {
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};

    let flags = if inheritable { HANDLE_FLAG_INHERIT } else { 0 };
    // SAFETY: the handle is open.
    let set = unsafe { SetHandleInformation(handle.as_raw_handle(), HANDLE_FLAG_INHERIT, flags) };
    assert_ne!(set, 0, "{}", std::io::Error::last_os_error());
}

/// Returns whether the handle `value` of process `pid` refers to the object of this process's
/// handle `value`, as a handle that the process inherited from this one, directly or through its
/// parent, does.
#[cfg(windows)]
fn holds(pid: u32, value: usize) -> bool {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::ptr;

    use windows_sys::Win32::Foundation::{
        CompareObjectHandles, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, PROCESS_DUP_HANDLE,
    };

    let ours = value as HANDLE;
    // SAFETY: OpenProcess has no memory-safety preconditions.
    let process = unsafe { OpenProcess(PROCESS_DUP_HANDLE, 0, pid) };
    assert!(
        !process.is_null(),
        "cannot open process {pid}: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: the handle is new, and nothing else owns it.
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    let mut copy = ptr::null_mut();
    // SAFETY: the process handle may duplicate the process's handles, the value is looked up in
    // that process only, and the output pointer is valid.
    let duplicated = unsafe {
        DuplicateHandle(
            process.as_raw_handle(),
            ours,
            GetCurrentProcess(),
            &mut copy,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    } != 0;
    // A value that names no handle of the process names none of ours.
    if !duplicated {
        return false;
    }
    // SAFETY: the copy is new, and nothing else owns it.
    let copy = unsafe { OwnedHandle::from_raw_handle(copy) };
    // SAFETY: both handles are open.
    unsafe { CompareObjectHandles(copy.as_raw_handle(), ours) != 0 }
}

/// Returns the IDs of the running processes that process `parent` started from an executable
/// named `name`.
#[cfg(windows)]
fn children_named(parent: u32, name: &str) -> Vec<u32> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    // SAFETY: CreateToolhelp32Snapshot has no memory-safety preconditions.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    assert_ne!(
        snapshot,
        INVALID_HANDLE_VALUE,
        "{}",
        std::io::Error::last_os_error()
    );
    // SAFETY: the snapshot handle is new, and nothing else owns it.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    // SAFETY: PROCESSENTRY32W is a plain C structure for which all-zero is a valid value.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut children = Vec::new();
    // SAFETY: the snapshot is open, and the entry holds its own size.
    let mut found = unsafe { Process32FirstW(snapshot.as_raw_handle(), &mut entry) } != 0;
    while found {
        let file = &entry.szExeFile;
        let length = file
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(file.len());
        if entry.th32ParentProcessID == parent
            && String::from_utf16_lossy(&file[..length]).eq_ignore_ascii_case(name)
        {
            children.push(entry.th32ProcessID);
        }
        // SAFETY: as above.
        found = unsafe { Process32NextW(snapshot.as_raw_handle(), &mut entry) } != 0;
    }
    children
}

#[cfg(windows)]
#[test]
fn a_run_passes_openvmm_no_handle_of_its_caller() {
    use std::os::windows::io::AsRawHandle;

    // A handle that the CLI inherits from its caller, as from a launcher that started it without a
    // handle list, and that `std::process::Command` would pass on to OpenVMM.
    let (_reader, unrelated) = std::io::pipe().unwrap();
    let root = Root::new();
    let path = |name: &str| root.path.join(name).to_str().unwrap().to_owned();
    let mut command = Command::new(NVX_RS);
    command
        .arg("--root")
        .arg(&root.path)
        .args([
            "sandbox",
            "run",
            "--layer",
            &format!(
                "distro,{},11111111-1111-1111-1111-111111111111",
                path("distro.erofs")
            ),
            "--scratch",
            &path("scratch.ext4"),
            "--entrypoint",
            "/bin/sleep",
            "--arg",
            "60",
        ])
        .current_dir(&root.path)
        .env_remove("NVX_OPENVMM")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    set_inheritable(&unrelated, true);
    let cli = command.spawn();
    set_inheritable(&unrelated, false);
    let mut cli = cli.unwrap();

    // Once OpenVMM has started, the CLI holds the handle, and OpenVMM does not.
    wait_for(&root.path.join("build").join("fake-openvmm-.json"));
    let value = unrelated.as_raw_handle() as usize;
    let openvmm = children_named(cli.id(), "openvmm.exe");
    let cli_holds = holds(cli.id(), value);
    let openvmm_holds: Vec<bool> = openvmm.iter().map(|&pid| holds(pid, value)).collect();
    for &pid in &openvmm {
        harness::kill(u64::from(pid));
    }
    if openvmm.is_empty() {
        let _ = cli.kill();
    }
    let _ = cli.wait();
    assert_eq!(openvmm.len(), 1, "{openvmm:?}");
    assert!(cli_holds, "the CLI did not inherit the handle");
    assert_eq!(
        openvmm_holds,
        [false],
        "OpenVMM inherited a handle of the CLI's caller"
    );
}

/// Writes a batch file below the root that leaves a marker if Windows runs it, and returns the
/// environment that names it as OpenVMM, the marker, and the error with which the CLI refuses it.
#[cfg(windows)]
fn batch_openvmm(root: &Root) -> (BTreeMap<String, String>, PathBuf, String) {
    let marker = root.path.join("ran");
    let wrapper = root.path.join("openvmm.cmd");
    fs::write(&wrapper, format!("@echo ran>\"{}\"\r\n", marker.display())).unwrap();
    let environment = BTreeMap::from([(
        "NVX_OPENVMM".to_owned(),
        wrapper.to_str().unwrap().to_owned(),
    )]);
    let refusal = format!(
        "error[unsupported]: cannot start the batch file {}: Windows runs batch files through \
         cmd.exe, which would read their arguments as shell syntax\n",
        wrapper.display()
    );
    (environment, marker, refusal)
}

#[cfg(windows)]
#[test]
fn a_run_refuses_a_batch_file_as_openvmm() {
    // cmd.exe would read the `&` of the scratch image's path as a second command.
    let root = Root::new();
    let (environment, marker, refusal) = batch_openvmm(&root);
    let scratch = root.path.join("scratch&echo.ext4");
    fs::copy(root.path.join("scratch.ext4"), &scratch).unwrap();
    let layer = format!(
        "distro,{},11111111-1111-1111-1111-111111111111",
        root.path.join("distro.erofs").display()
    );
    let arguments: Vec<String> = [
        "sandbox",
        "run",
        "--layer",
        &layer,
        "--scratch",
        scratch.to_str().unwrap(),
        "--entrypoint",
        "/bin/true",
    ]
    .map(str::to_owned)
    .to_vec();
    let output = nvx_rs(&root, &arguments, &environment);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output.stderr));
    assert_eq!(text(&output.stderr), refusal);
    thread::sleep(Duration::from_millis(500));
    assert!(!marker.exists());
}

#[test]
fn artifacts_default_to_the_checkout_that_holds_the_executable() {
    let output = Command::new(NVX_RS)
        .arg("artifacts")
        .env_remove("NVX_OPENVMM")
        .output()
        .unwrap();
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let in_checkout = fs::canonicalize(manifest_dir.join("target"))
        .is_ok_and(|target| canonical(Path::new(NVX_RS)).starts_with(canonical(&target)));
    if in_checkout {
        assert!(output.status.success(), "{}", text(&output.stderr));
        let paths: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(paths["layout"], "repository");
        assert_eq!(
            canonical(Path::new(paths["root"].as_str().unwrap())),
            canonical(manifest_dir.parent().unwrap())
        );
    } else {
        // A build elsewhere, as with CARGO_TARGET_DIR, needs --root.
        assert_eq!(output.status.code(), Some(1));
        assert!(
            text(&output.stderr)
                .starts_with("error[artifact-missing]: cannot locate the NVX artifacts")
        );
    }
}

#[test]
fn a_relative_openvmm_override_is_relative_to_the_working_directory() {
    let root = Root::new();
    let output = Command::new(NVX_RS)
        .args(["--root", ".", "artifacts"])
        .current_dir(&root.path)
        .env("NVX_OPENVMM", Path::new("alt").join("openvmm"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output.stderr));
    let paths: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(paths["root"], root.text());
    assert_eq!(
        paths["openvmm"],
        root.path.join("alt").join("openvmm").to_str().unwrap()
    );
}

#[test]
fn top_level_usage_errors_exit_with_status_2() {
    let usage = "usage: nvx-rs [-h] [--root PATH] {artifacts,download,egress,run,sandbox} ...\n";
    for (arguments, message) in [
        (
            &[][..],
            "nvx-rs: error[argument-missing]: the following arguments are required: command\n",
        ),
        (
            &["--root"][..],
            "nvx-rs: error[argument-missing]: argument --root: expected one argument\n",
        ),
        (
            &["build"][..],
            "nvx-rs: error[argument-invalid]: argument command: invalid choice: \"build\" (choose \
             from \"artifacts\", \"download\", \"egress\", \"run\", \"sandbox\")\n",
        ),
        (
            &["artifacts", "--bogus"][..],
            "nvx-rs: error[argument-unrecognized]: unrecognized arguments: --bogus\n",
        ),
        // No option names the program any more.
        (
            &["--prog", "nvx.py", "sandbox"][..],
            "nvx-rs: error[argument-invalid]: argument command: invalid choice: \"nvx.py\" \
             (choose from \"artifacts\", \"download\", \"egress\", \"run\", \"sandbox\")\n",
        ),
        (
            &["--prog=nvx.py", "artifacts"][..],
            "nvx-rs: error[argument-unrecognized]: unrecognized arguments: --prog=nvx.py\n",
        ),
    ] {
        let output = Command::new(NVX_RS).args(arguments).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
        assert_eq!(
            text(&output.stderr),
            format!("{usage}{message}"),
            "{arguments:?}"
        );
    }
    let output = Command::new(NVX_RS)
        .args(["egress", "compile"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        text(&output.stderr),
        "usage: nvx-rs egress compile [-h] PATH\n\
         nvx-rs egress compile: error[argument-missing]: the following arguments are required: \
         PATH\n"
    );
}

#[test]
fn usage_errors_print_the_one_line_usage_of_their_parser() {
    for (arguments, expected) in [
        (
            &["sandbox", "--workload-user", "0:0"][..],
            "usage: nvx-rs sandbox [{run,provision,start,exec,stop,deprovision}] [OPTIONS]\n\
             nvx-rs sandbox: error[argument-invalid]: argument --workload-user: --workload-user \
             UID must be between 1 and 4294967294\n",
        ),
        (
            &["run", "--processors", "3"][..],
            "usage: nvx-rs run [OPTIONS]\n\
             nvx-rs run: error[argument-invalid]: argument --processors: invalid choice: \"3\" \
             (choose from \"1\", \"2\", \"4\", \"8\")\n",
        ),
    ] {
        let output = Command::new(NVX_RS).args(arguments).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
        assert_eq!(text(&output.stderr), expected, "{arguments:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}");
    }
    // The help starts with the same usage and lists the options after it.
    let output = Command::new(NVX_RS)
        .args(["artifacts", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(text(&output.stdout).starts_with("usage: nvx-rs artifacts [-h]\n\nPrint the paths"));
}

/// The host CPU as the host OS reports it, independently of the CPUID that `nvx-rs` reads: its
/// vendor and display family, model, and stepping, from the first processor of `/proc/cpuinfo` on
/// Linux, and from `PROCESSOR_IDENTIFIER`, such as `Intel64 Family 6 Model 85 Stepping 4,
/// GenuineIntel`, on Windows.
fn reported_cpu() -> (String, u32, u32, u32) {
    if cfg!(windows) {
        let identifier = std::env::var("PROCESSOR_IDENTIFIER").unwrap();
        let (description, vendor) = identifier.split_once(", ").unwrap();
        let words: Vec<&str> = description.split_whitespace().collect();
        let number = |name: &str| -> u32 {
            let position = words.iter().position(|word| *word == name).unwrap();
            words[position + 1].parse().unwrap()
        };
        return (
            vendor.trim().to_owned(),
            number("Family"),
            number("Model"),
            number("Stepping"),
        );
    }
    let cpuinfo = fs::read_to_string("/proc/cpuinfo").unwrap();
    let field = |name: &str| -> String {
        cpuinfo
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                (key.trim() == name).then(|| value.trim().to_owned())
            })
            .unwrap()
    };
    (
        field("vendor_id"),
        field("cpu family").parse().unwrap(),
        field("model").parse().unwrap(),
        field("stepping").parse().unwrap(),
    )
}

/// Returns whether a built-in CPU profile serves the CPU of `vendor` with the display `family`,
/// `model`, and `stepping`, by the copy of OpenVMM's catalog that `nvx-rs` reads.
fn served(vendor: &str, family: u32, model: u32, stepping: u32) -> bool {
    let catalog: Value =
        serde_json::from_str(include_str!("../src/host/cli/cpu_generations.json")).unwrap();
    catalog.as_array().unwrap().iter().any(|profile| {
        profile["vendor"] == vendor
            && profile["generation"]["cpus"]
                .as_array()
                .unwrap()
                .iter()
                .any(|cpu| {
                    let steppings = cpu["steppings"].as_array().unwrap();
                    cpu["family"] == family
                        && cpu["model"] == model
                        && steppings[0].as_u64().unwrap() <= u64::from(stepping)
                        && u64::from(stepping) <= steppings[1].as_u64().unwrap()
                })
    })
}

#[test]
fn run_guides_failed_cold_boots_on_cpus_that_no_built_in_profile_serves() {
    // nvx-rs run identifies the host's CPU with CPUID, as OpenVMM does and as the host OS reports
    // it, and guides a failed cold boot only where no built-in profile serves the CPU: what
    // follows OpenVMM's output depends on the host that runs the test.
    let (vendor, family, model, stepping) = reported_cpu();
    let unserved = !served(&vendor, family, model, stepping);
    let host_profiles = ["GenuineIntel", "AuthenticAMD"].contains(&vendor.as_str());
    let root = Root::new();
    let arguments = |arguments: &[&str]| -> Vec<String> {
        std::iter::once("run")
            .chain(arguments.iter().copied())
            .map(str::to_owned)
            .collect()
    };
    // The guidance asks for a fingerprint from the OpenVMM that the run ran, as the run quoted it.
    let dry_run = text(&run(&root, &arguments(&["--dry-run"])).stdout);
    let (openvmm, _) = dry_run
        .strip_prefix(">> ")
        .and_then(|command| command.split_once(" --single-process "))
        .unwrap();
    let default = if cfg!(windows) { "whp" } else { "kvm" };
    // OpenVMM fails the cold boot, or refuses its command line with the status of a usage error;
    // the guidance follows what OpenVMM printed, for auto, the default, on every unserved CPU, and
    // for host on a CPU of a vendor that host profiles do not serve either.
    for (options, status, printed, guided, hypervisor) in [
        (
            &["--cmdline", "fake_exit_status=1"][..],
            1,
            "",
            unserved,
            default,
        ),
        (
            &["--cmdline", "fake_exit_status=2"][..],
            2,
            "",
            unserved,
            default,
        ),
        (
            &["--host-loopback-forward", "tcp:8080:80"][..],
            2,
            "openvmm: --host-loopback-forward requires explicit --host-loopback allow\n",
            unserved,
            default,
        ),
        (
            &[
                "--cpu-profile",
                "auto",
                "--hypervisor",
                "mshv",
                "--cmdline",
                "fake_exit_status=1",
            ][..],
            1,
            "",
            unserved,
            "mshv",
        ),
        (
            &["--cpu-profile", "host", "--cmdline", "fake_exit_status=1"][..],
            1,
            "",
            unserved && !host_profiles,
            default,
        ),
        (
            &[
                "--cpu-profile",
                "intel.alderlake.v1",
                "--cmdline",
                "fake_exit_status=1",
            ][..],
            1,
            "",
            false,
            default,
        ),
    ] {
        let output = run(&root, &arguments(options));
        assert_eq!(output.status.code(), Some(status), "{options:?}");
        assert!(text(&output.stdout).starts_with(">> "), "{options:?}");
        let stderr = text(&output.stderr);
        let guidance = stderr
            .strip_prefix(printed)
            .unwrap_or_else(|| panic!("{options:?}: {stderr}"));
        if !guided {
            assert_eq!(guidance, "", "{options:?}");
            continue;
        }
        assert!(
            guidance.starts_with(&format!(
                "nvx: no built-in CPU profile serves this host's CPU, {vendor} \
                 {family}/{model}/{stepping}"
            )),
            "{options:?}: {guidance}"
        );
        assert!(
            guidance.ends_with('\n') && guidance.lines().all(|line| line.starts_with("nvx: ")),
            "{options:?}: {guidance}"
        );
        let fingerprint = format!(
            "\nnvx:   {openvmm} --hypervisor {hypervisor} --cpu-fingerprint fingerprint.json\n"
        );
        assert_eq!(
            guidance.contains(&fingerprint),
            host_profiles,
            "{options:?}: {guidance}"
        );
        assert_eq!(
            guidance.contains("E_PROFILE_HOST_UNKNOWN"),
            !host_profiles,
            "{options:?}: {guidance}"
        );
    }
    // A cold boot that succeeds, a dry run, the restores that fail, the options that nvx-rs
    // refuses, its usage errors, and the help print none.
    let snapshot = root.path.join("snapshot");
    fs::create_dir(&snapshot).unwrap();
    let snapshot = snapshot.to_str().unwrap();
    for (options, status) in [
        (&[][..], 0),
        (&["--cmdline", "fake_exit_status=1", "--dry-run"][..], 0),
        (
            &[
                "--restore-snapshot",
                snapshot,
                "--cmdline",
                "fake_exit_status=1",
            ][..],
            1,
        ),
        (&["--restore-snapshot", "missing"][..], 1),
        (&["--net", "10.0.0.2/24"][..], 1),
        (
            &["--mount", "/data", "--cmdline", "fake_exit_status=1"][..],
            1,
        ),
        (&["--processors", "3"][..], 2),
        (&["--bogus"][..], 2),
        (&["--help"][..], 0),
    ] {
        let output = run(&root, &arguments(options));
        assert_eq!(output.status.code(), Some(status), "{options:?}");
        assert!(
            !text(&output.stderr).contains("nvx: "),
            "{options:?}: {}",
            text(&output.stderr)
        );
    }
    // Nor does OpenVMM that does not start.
    let environment = BTreeMap::from([(
        "NVX_OPENVMM".to_owned(),
        root.path.join("single.txt").to_str().unwrap().to_owned(),
    )]);
    let output = nvx_rs(&root, &arguments(&[]), &environment);
    assert_eq!(output.status.code(), Some(1));
    let stderr = text(&output.stderr);
    assert!(
        stderr.starts_with("error[io]: ") && !stderr.contains("nvx: "),
        "{stderr}"
    );
}

#[test]
fn run_boots_each_guest_with_its_default_memory() {
    let root = Root::new();
    for guest in nvx::host::artifacts::Guest::ALL {
        let initramfs = root.path.join("build").join(guest.initramfs_name());
        fs::write(initramfs, "initramfs\n").unwrap();
        let arguments = ["run", "--guest", guest.name(), "--dry-run"].map(str::to_owned);
        let output = run(&root, &arguments);
        assert!(output.status.success(), "{}", text(&output.stderr));
        let memory = format!(" --memory {}M ", guest.default_memory_mib());
        assert!(text(&output.stdout).contains(&memory), "{guest}");
    }
}

#[test]
fn run_shares_relative_roots_with_absolute_policy_paths() {
    // nvx-rs run passes the host path of each share as given, so the children of an aggregate may
    // be relative to the working directory, while their policy paths must be absolute: OpenVMM
    // attributes each one to the share whose root contains it once it has resolved both.
    let root = Root::new();
    let path = |parts: &[&str]| {
        parts
            .iter()
            .fold(root.path.clone(), |path, part| path.join(part))
            .to_str()
            .unwrap()
            .to_owned()
    };
    let arguments = [
        "run".to_owned(),
        "--mount".to_owned(),
        "/data,share,rw".to_owned(),
        "--mount".to_owned(),
        "/other,share2".to_owned(),
        "--mount-deny".to_owned(),
        path(&["share", "hidden"]),
        "--mount-allow".to_owned(),
        path(&["share", "hidden", "shown"]),
    ];
    let output = run(&root, &arguments);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let stdout = text(&output.stdout);
    let child = nvx::host::sandbox::mount::aggregate_child_name(0, "/data");
    assert!(
        stdout.contains(&format!(" --mount-child {child},share,rw ")),
        "{stdout}"
    );
    assert!(
        stdout.ends_with("\nopenvmm: booted the microVM\n"),
        "{stdout}"
    );
}

#[cfg(unix)]
mod interrupts {
    use std::time::Instant;

    use super::*;

    /// Sends SIGINT once OpenVMM has started, to the CLI alone or, as a terminal's Ctrl-C does,
    /// to its whole process group, which a run's OpenVMM shares, and returns the CLI's output.
    fn interrupt(
        root: &Root,
        arguments: &[String],
        started: &Path,
        group: bool,
    ) -> (Output, Duration) {
        use std::os::unix::process::CommandExt;

        let mut command = Command::new(NVX_RS);
        command
            .arg("--root")
            .arg(&root.path)
            .args(arguments)
            .current_dir(&root.path)
            .env_remove("NVX_OPENVMM")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if group {
            command.process_group(0);
        }
        let child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !started.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        thread::sleep(Duration::from_millis(200));
        let interrupted = Instant::now();
        let pid = nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap());
        if group {
            nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGINT).unwrap();
        } else {
            nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT).unwrap();
        }
        let output = child.wait_with_output().unwrap();
        (output, interrupted.elapsed())
    }

    /// Returns the arguments of a one-shot run of `/bin/sleep 60` in the fake OpenVMM, and the
    /// file that the fake writes when it starts.
    fn sleeping_run(root: &Root) -> (Vec<String>, PathBuf) {
        let path = |name: &str| root.path.join(name).to_str().unwrap().to_owned();
        let arguments: Vec<String> = [
            "sandbox",
            "run",
            "--layer",
            &format!(
                "distro,{},11111111-1111-1111-1111-111111111111",
                path("distro.erofs")
            ),
            "--scratch",
            &path("scratch.ext4"),
            "--entrypoint",
            "/bin/sleep",
            "--arg",
            "60",
        ]
        .map(str::to_owned)
        .to_vec();
        // The fake records its command line when it starts.
        (
            arguments,
            root.path.join("build").join("fake-openvmm-.json"),
        )
    }

    #[test]
    fn interrupting_a_run_kills_openvmm_and_exits_with_130() {
        let root = Root::new();
        let (arguments, started) = sleeping_run(&root);
        let (output, elapsed) = interrupt(&root, &arguments, &started, false);
        assert_eq!(output.status.code(), Some(130));
        assert_eq!(text(&output.stderr), "Interrupted\n");
        assert!(text(&output.stdout).starts_with(">> "));
        assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
    }

    #[test]
    fn interrupting_a_cold_boot_ends_it_without_guidance() {
        // The fake OpenVMM delays its boot, which Ctrl-C ends, alone or with OpenVMM, as the
        // terminal's does; nvx-rs run then exits as interrupted, without the guidance that it
        // prints after a failed cold boot on a CPU that no built-in profile serves.
        let root = Root::new();
        let arguments = ["run", "--cmdline", "fake_boot_delay_ms=60000"].map(str::to_owned);
        let started = root.path.join("build").join("fake-openvmm-.json");
        for group in [false, true] {
            let _ = fs::remove_file(&started);
            let (output, elapsed) = interrupt(&root, &arguments, &started, group);
            assert_eq!(output.status.code(), Some(130), "{group}");
            assert_eq!(text(&output.stderr), "Interrupted\n", "{group}");
            assert!(text(&output.stdout).starts_with(">> "), "{group}");
            assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
        }
    }

    #[test]
    fn interrupting_a_run_ends_it_as_interrupted_even_if_openvmm_exits_first() {
        // A terminal's Ctrl-C reaches OpenVMM too, which the fake lets end it at once; Python's
        // wait raises KeyboardInterrupt all the same, rather than return OpenVMM's status.
        let root = Root::new();
        let (arguments, started) = sleeping_run(&root);
        let (output, elapsed) = interrupt(&root, &arguments, &started, true);
        assert_eq!(output.status.code(), Some(130));
        assert_eq!(text(&output.stderr), "Interrupted\n");
        assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
    }

    #[test]
    fn interrupting_an_exec_leaves_the_sandbox_running() {
        let root = Root::new();
        let output = run(&root, &provision(&root, "state", ""));
        assert!(output.status.success(), "{}", text(&output.stderr));
        let output = run(&root, &lifecycle("start", &root, "state"));
        assert!(output.status.success(), "{}", text(&output.stderr));
        let mut arguments = lifecycle("exec", &root, "state");
        arguments.extend(
            [
                "--entrypoint",
                "/bin/sh",
                "--arg=-c",
                "--arg",
                "sleep 60000",
            ]
            .map(str::to_owned),
        );
        let started = root.path.join("state").join("runtime.json");
        let (output, elapsed) = interrupt(&root, &arguments, &started, false);
        assert_eq!(output.status.code(), Some(130));
        assert_eq!(text(&output.stderr), "Interrupted\n");
        assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
        for operation in ["stop", "deprovision"] {
            let output = run(&root, &lifecycle(operation, &root, "state"));
            assert!(
                output.status.success(),
                "{operation}: {}",
                text(&output.stderr)
            );
        }
    }

    #[test]
    fn interrupting_a_provision_that_waits_for_a_download_provisions_nothing() {
        use std::io::{BufRead, BufReader, Read};

        // A provision in a checkout waits for a download that holds the lock of its downloads,
        // and Ctrl-C ends the wait and the command, which records no sandbox.
        let root = Root::new();
        root.write("SOURCE-MANIFEST.json", "{}\n");
        fs::create_dir(root.path.join(".git")).unwrap();
        root.write("build/.nvx-download.lock", "");
        let download = fs::File::open(root.path.join("build").join(".nvx-download.lock")).unwrap();
        download.lock().unwrap();
        let mut child = Command::new(NVX_RS)
            .arg("--root")
            .arg(&root.path)
            .args(provision(&root, "state", ""))
            .current_dir(&root.path)
            .env_remove("NVX_OPENVMM")
            .env_remove("NVX_AUTO_DOWNLOAD")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stderr = BufReader::new(child.stderr.take().unwrap());
        let mut waiting = String::new();
        stderr.read_line(&mut waiting).unwrap();
        assert!(
            waiting.starts_with(">> waiting for another download into "),
            "{waiting}"
        );
        let pid = nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap());
        nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT).unwrap();
        let status = child.wait().unwrap();
        let mut rest = String::new();
        stderr.read_to_string(&mut rest).unwrap();
        assert_eq!(status.code(), Some(130), "{rest}");
        assert_eq!(rest, "Interrupted\n");
        assert!(!root.path.join("state").exists());
        drop(download);
    }

    /// Waits until `child` writes to `output`, a pipe that nothing reads and that the write fills,
    /// then sends it SIGINT, and returns how it ended and what it wrote to standard error, without
    /// draining the pipe.
    fn interrupt_full_pipe(
        mut child: std::process::Child,
        output: impl std::os::fd::AsFd,
    ) -> (std::process::ExitStatus, String) {
        use std::io::Read;

        use nix::poll::{PollFd, PollFlags, poll};

        let mut ready = [PollFd::new(output.as_fd(), PollFlags::POLLIN)];
        assert_eq!(
            poll(&mut ready, 30_000u16).unwrap(),
            1,
            "nothing was written"
        );
        thread::sleep(Duration::from_millis(200));
        let mut stderr = child.stderr.take().unwrap();
        let errors = thread::spawn(move || {
            let mut errors = Vec::new();
            stderr.read_to_end(&mut errors).map(|_| errors)
        });
        let interrupted = Instant::now();
        let pid = nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap());
        nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT).unwrap();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if interrupted.elapsed() > Duration::from_secs(10) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("Ctrl-C did not end the write to a full pipe");
            }
            thread::sleep(Duration::from_millis(20));
        };
        (status, text(&errors.join().unwrap().unwrap()))
    }

    #[test]
    fn interrupting_an_exec_ends_its_write_to_a_full_pipe() {
        // The CLI writes a workload's output once the workload ends. Like Python's, the write
        // ends at Ctrl-C, also while it waits for a pipe's reader that does not read.
        let root = Root::new();
        for arguments in [
            provision(&root, "state", ""),
            lifecycle("start", &root, "state"),
        ] {
            let output = run(&root, &arguments);
            assert!(output.status.success(), "{}", text(&output.stderr));
        }
        let mut arguments = lifecycle("exec", &root, "state");
        arguments.extend(
            [
                "--entrypoint",
                "/bin/sh",
                "--arg=-c",
                "--arg",
                "flood 262144",
            ]
            .map(str::to_owned),
        );
        let mut child = Command::new(NVX_RS)
            .arg("--root")
            .arg(&root.path)
            .args(&arguments)
            .current_dir(&root.path)
            .env_remove("NVX_OPENVMM")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // The output is four times the size of a pipe.
        let stdout = child.stdout.take().unwrap();
        let (status, errors) = interrupt_full_pipe(child, stdout);
        assert_eq!(status.code(), Some(130));
        assert_eq!(errors, "Interrupted\n");
        for operation in ["stop", "deprovision"] {
            let output = run(&root, &lifecycle(operation, &root, "state"));
            assert!(
                output.status.success(),
                "{operation}: {}",
                text(&output.stderr)
            );
        }
    }

    #[test]
    fn interrupting_help_ends_its_write_to_a_full_pipe() {
        // Help also ends as interrupted, as `KeyboardInterrupt` ends Python's.
        let help = Command::new(NVX_RS)
            .args(["sandbox", "--help"])
            .output()
            .unwrap();
        assert!(help.stdout.len() > 4096, "the help fits in one page");
        let (reader, writer) = std::io::pipe().unwrap();
        // A pipe of one page, which the help, longer than that, fills.
        nix::fcntl::fcntl(&writer, nix::fcntl::FcntlArg::F_SETPIPE_SZ(4096)).unwrap();
        let child = Command::new(NVX_RS)
            .args(["sandbox", "--help"])
            .stdin(Stdio::null())
            .stdout(writer)
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (status, errors) = interrupt_full_pipe(child, reader);
        assert_eq!(status.code(), Some(130));
        assert_eq!(errors, "Interrupted\n");
    }
}
