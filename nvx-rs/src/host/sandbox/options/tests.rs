//! The `nvx.py sandbox` and `nvx.py run` option tests of `scripts/test_nvx_tools.py`, ported.

use std::fs;
use std::path::Path;

use proptest::prelude::*;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::*;
use crate::ErrorKind;
use crate::host::python::fs::resolve;
use crate::host::sandbox::layer::LayerRole;
use crate::host::sandbox::mount::{aggregate_child_name, plan_mounts, validate_mounts};
use crate::host::sandbox::{SandboxLayer, SandboxSettings};

fn message<T: std::fmt::Debug>(result: Result<T>) -> String {
    let error = result.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
    error.to_string()
}

fn launching(operation: SandboxOperation) -> SandboxOptions {
    SandboxOptions {
        layers: true,
        scratch: true,
        ..SandboxOptions::new(operation)
    }
}

#[test]
fn sandbox_rejects_systemd_entrypoint() {
    for entrypoint in SYSTEMD_ENTRYPOINTS {
        for operation in [
            SandboxOperation::Run,
            SandboxOperation::Provision,
            SandboxOperation::Exec,
        ] {
            let options = SandboxOptions {
                entrypoint: entrypoint.to_owned(),
                ..launching(operation)
            };
            assert_eq!(
                message(options.check()),
                "systemd entrypoints are unsupported by the sandbox security profile"
            );
        }
        let stop = SandboxOptions {
            entrypoint: entrypoint.to_owned(),
            ..SandboxOptions::new(SandboxOperation::Stop)
        };
        assert_eq!(stop.check().unwrap(), None);
    }
    let init = SandboxOptions {
        entrypoint: "/sbin/init".to_owned(),
        ..launching(SandboxOperation::Run)
    };
    assert_eq!(init.check().unwrap(), None);
}

/// A distro layer in a temporary directory.
struct Layer {
    /// The temporary directory.
    directory: tempfile::TempDir,
}

impl Layer {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("distro.erofs"), b"distro").unwrap();
        Self { directory }
    }

    fn path(&self, name: &str) -> String {
        PurePath::native(self.directory.path().to_str().unwrap())
            .join(name)
            .as_str()
            .to_owned()
    }

    fn launch(&self, entrypoint: &str) -> SandboxLaunch {
        SandboxLaunch::new(
            vec![SandboxLayer::new(
                LayerRole::Distro,
                &self.path("distro.erofs"),
                "11111111-1111-1111-1111-111111111111",
            )],
            &self.path("scratch.ext4"),
            SandboxSettings {
                entrypoint: entrypoint.to_owned(),
                ..SandboxSettings::default()
            },
        )
        .unwrap()
    }

    fn manifest(&self, text: &str) {
        fs::write(self.path("distro.erofs.manifest.json"), text).unwrap();
    }
}

fn distro_sha256() -> String {
    Sha256::digest(b"distro")
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn exec_environment_limits() {
    for entries in [
        Vec::new(),
        vec!["EMPTY=".to_owned(), "EQUALS=a=b".to_owned()],
        (0..256).map(|index| format!("K{index}=value")).collect(),
        vec![format!("K={}", "x".repeat(4094))],
        vec![format!("K={}", "\u{e9}".repeat(2047))],
    ] {
        validate_exec_environment(&entries).unwrap();
    }
    for entries in [
        vec!["".to_owned()],
        vec!["NO_EQUALS".to_owned()],
        vec!["=no-name".to_owned()],
        vec!["K=a\0b".to_owned()],
        vec!["K\0=value".to_owned()],
        vec!["K=first".to_owned(), "K=second".to_owned()],
        (0..257).map(|index| format!("K{index}=value")).collect(),
        vec![format!("K={}", "x".repeat(4095))],
        vec![format!("K={}", "\u{e9}".repeat(2048))],
    ] {
        assert_eq!(
            validate_exec_environment(&entries).unwrap_err().kind(),
            ErrorKind::Unsupported,
        );
    }
}

#[test]
fn environment_files_hold_a_json_array_of_strings() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("environment.json");
    let path = path.to_str().unwrap();
    for (text, entries) in [
        ("[]", vec![]),
        (r#"["K=V","EMPTY="]"#, vec!["K=V", "EMPTY="]),
        (r#"["K=a=b","ESCAPED=\u0061"]"#, vec!["K=a=b", "ESCAPED=a"]),
    ] {
        fs::write(path, text).unwrap();
        assert_eq!(exec_environment_from_file(path).unwrap(), entries);
    }
    for text in [
        "",
        "null",
        "{}",
        r#""K=V""#,
        "[1]",
        "[true]",
        "[null]",
        "[[]]",
        "[{}]",
        r#"["=V"]"#,
        r#"["K"]"#,
        r#"["K=\u0000"]"#,
        r#"["K=a","K=b"]"#,
        r#"["K=V",]"#,
        r#"["K=V"] trailing"#,
    ] {
        fs::write(path, text).unwrap();
        assert_eq!(
            exec_environment_from_file(path).unwrap_err().kind(),
            ErrorKind::Unsupported,
            "{text}",
        );
    }
    fs::write(path, b"[\xff]").unwrap();
    assert_eq!(
        exec_environment_from_file(path).unwrap_err().kind(),
        ErrorKind::Unsupported,
    );
    let exact = format!("[]{}", " ".repeat(MAX_ENVIRONMENT_FILE_BYTES - 2));
    fs::write(path, &exact).unwrap();
    assert_eq!(
        exec_environment_from_file(path).unwrap(),
        Vec::<String>::new()
    );
    fs::write(path, format!("{exact} ")).unwrap();
    assert_eq!(
        exec_environment_from_file(path).unwrap_err().kind(),
        ErrorKind::Unsupported,
    );
    fs::remove_file(path).unwrap();
    assert_eq!(
        exec_environment_from_file(path).unwrap_err().kind(),
        ErrorKind::Unsupported,
    );
}

#[test]
fn distro_manifest_binds_the_layer() {
    let layer = Layer::new();
    let launch = layer.launch("/bin/sh");
    let valid = json!({
        "format": 1,
        "artifact": "distro.erofs",
        "artifact_sha256": distro_sha256(),
        "packages": [{"name": "bash"}, {"name": "coreutils"}],
    });
    layer.manifest(&valid.to_string());
    let binding = validate_sandbox_systemd_policy(&launch).unwrap();
    assert_eq!(binding.identity(), "11111111-1111-1111-1111-111111111111");
    assert_eq!(binding.format(), super::super::DistroFormat::Erofs);
    assert!(binding.manifest().is_some());
    binding
        .check(
            launch.distro(),
            super::super::distro::DISTRO_POLICY_REVISION,
        )
        .unwrap();
    for (field, value) in [
        ("format", json!(2)),
        ("format", json!("1")),
        ("artifact", json!("other.erofs")),
        ("artifact_sha256", json!("0".repeat(64))),
        ("packages", json!(null)),
        ("packages", json!([1])),
        ("packages", json!([{}])),
        ("packages", json!([{"name": 1}])),
        ("packages", json!([{"name": "systemd"}, 1])),
    ] {
        let mut manifest = valid.clone();
        manifest[field] = value;
        layer.manifest(&manifest.to_string());
        assert_eq!(
            validate_sandbox_systemd_policy(&launch).unwrap_err().kind(),
            ErrorKind::Unsupported,
            "{field}: {manifest}",
        );
    }
    for field in ["format", "artifact", "artifact_sha256", "packages"] {
        let mut manifest = valid.clone();
        manifest.as_object_mut().unwrap().remove(field);
        layer.manifest(&manifest.to_string());
        assert_eq!(
            validate_sandbox_systemd_policy(&launch).unwrap_err().kind(),
            ErrorKind::Unsupported,
        );
    }
    layer.manifest(&valid.to_string());
    fs::write(layer.path("distro.erofs"), b"changed distro").unwrap();
    assert_eq!(
        validate_sandbox_systemd_policy(&launch).unwrap_err().kind(),
        ErrorKind::Unsupported,
    );
}

#[test]
fn systemd_entrypoints_and_images_are_refused_independently() {
    let layer = Layer::new();
    for packages in [json!([]), json!([{"name": "systemd"}])] {
        layer.manifest(
            &json!({
                "format": 1,
                "artifact": "distro.erofs",
                "artifact_sha256": distro_sha256(),
                "packages": packages,
            })
            .to_string(),
        );
        for entrypoint in ["/usr/lib/systemd/systemd", "/lib/systemd/systemd"] {
            assert_eq!(
                validate_sandbox_systemd_policy(&layer.launch(entrypoint))
                    .unwrap_err()
                    .kind(),
                ErrorKind::Unsupported,
            );
        }
        let result = validate_sandbox_systemd_policy(&layer.launch("/sbin/init"));
        assert_eq!(result.is_ok(), packages.as_array().unwrap().is_empty());
    }
}

#[test]
fn policy_paths_need_a_preceding_mount_only_with_several_shares() {
    let first = "/first,host-first,rw".to_owned();
    let second = "/second,host-second,rw".to_owned();
    let option = |preceding_mount| PolicyPathOption {
        kind: PolicyKind::Denied,
        preceding_mount,
        path: "secret".to_owned(),
    };
    let mounts = sandbox_mounts(std::slice::from_ref(&first), &[option(None)], None).unwrap();
    assert_eq!(mounts[0].policy().denied, ["secret"]);
    for policy in [option(None), option(Some(2))] {
        assert_eq!(
            sandbox_mounts(&[first.clone(), second.clone()], &[policy], None)
                .unwrap_err()
                .kind(),
            ErrorKind::Unsupported,
        );
    }
    assert_eq!(
        sandbox_mounts(&[], &[option(None)], None)
            .unwrap_err()
            .kind(),
        ErrorKind::Unsupported,
    );
}

proptest! {
    #[test]
    fn policy_paths_attach_to_their_shares(
        events in prop::collection::vec((any::<bool>(), 0_u8..3), 0..60),
    ) {
        let mut mounts = Vec::new();
        let mut options = Vec::new();
        let mut expected = Vec::new();
        for (sequence, (next_mount, kind)) in std::iter::once((true, 0)).chain(events).enumerate() {
            if next_mount || mounts.is_empty() {
                let index = mounts.len();
                mounts.push(format!("/share-{index},host-{index},rw"));
                expected.push(MountPolicy::default());
                options.push(PolicyPathOption {
                    kind: PolicyKind::Denied,
                    preceding_mount: Some(index),
                    path: "hidden".to_owned(),
                });
                expected[index].denied.push("hidden".to_owned());
            }
            let index = mounts.len() - 1;
            let path = format!("hidden/path-{sequence}");
            let (kind, paths) = match kind {
                0 => (PolicyKind::Denied, &mut expected[index].denied),
                1 => (PolicyKind::Allowed, &mut expected[index].allowed),
                _ => (PolicyKind::Writable, &mut expected[index].writable),
            };
            paths.push(path.clone());
            options.push(PolicyPathOption { kind, preceding_mount: Some(index), path });
        }
        let actual = sandbox_mounts(&mounts, &options, None).unwrap();
        prop_assert_eq!(actual.len(), expected.len());
        for (index, (mount, policy)) in actual.iter().zip(expected).enumerate() {
            prop_assert_eq!(mount.guest_target(), format!("/share-{index}"));
            prop_assert_eq!(mount.policy(), &policy);
            prop_assert_eq!(mount.owner(), MountOwner::Vmm);
            prop_assert_eq!(mount.kind(), MountKind::Directory);
        }
    }

    #[test]
    fn run_share_targets_are_disjoint(suffix in "[a-z]{1,20}") {
        let first = format!("/first-{suffix}");
        let second = format!("/second-{suffix}");
        validate_run_share_targets(&[&first, &second]).unwrap();
        for targets in [
            vec![first.as_str(), first.as_str()],
            vec![first.as_str(), "/run/nvx/shares"],
        ] {
            prop_assert!(validate_run_share_targets(&targets).is_err());
        }
        let child = format!("{first}/child");
        prop_assert!(validate_run_share_targets(&[&first, &child]).is_err());
        prop_assert!(validate_run_share_targets(&[&child, &first]).is_err());
    }
}

#[test]
fn sandbox_allows_non_systemd_sbin_init() {
    let layer = Layer::new();
    validate_sandbox_systemd_policy(&layer.launch("/sbin/init")).unwrap();
    assert_eq!(
        message(validate_sandbox_systemd_policy(
            &layer.launch("/lib/systemd/systemd")
        )),
        "systemd entrypoints are unsupported by the sandbox security profile"
    );
}

#[test]
fn the_entrypoint_check_needs_no_manifest_and_precedes_the_distro_verification() {
    let layer = Layer::new();
    for entrypoint in SYSTEMD_ENTRYPOINTS {
        assert_eq!(
            message(validate_sandbox_entrypoint(entrypoint)),
            "systemd entrypoints are unsupported by the sandbox security profile"
        );
    }
    validate_sandbox_entrypoint("/sbin/init").unwrap();
    // A layer without a manifest passes the distro verification unread, and its binding records
    // the manifest's absence, but the entrypoint check still refuses systemd.
    let binding = validate_sandbox_systemd_policy(&layer.launch("/sbin/init")).unwrap();
    assert_eq!(binding.manifest(), None);
    assert_eq!(
        message(validate_sandbox_systemd_policy(
            &layer.launch("/usr/lib/systemd/systemd")
        )),
        "systemd entrypoints are unsupported by the sandbox security profile"
    );
    // The entrypoint check precedes the manifest, which it never reads.
    layer.manifest("not JSON");
    assert_eq!(
        message(validate_sandbox_systemd_policy(
            &layer.launch("/lib/systemd/systemd")
        )),
        "systemd entrypoints are unsupported by the sandbox security profile"
    );
}

#[test]
fn sandbox_rejects_systemd_from_bound_distro_metadata() {
    let layer = Layer::new();
    let digest = distro_sha256();
    let manifest = |format: &str, artifact: &str, sha256: &str, packages: &str| {
        layer.manifest(&format!(
            "{{\"format\": {format}, \"artifact\": {artifact}, \"artifact_sha256\": {sha256}, \
             \"packages\": {packages}}}"
        ));
        validate_sandbox_systemd_policy(&layer.launch("/sbin/init"))
    };
    let sha256 = format!("\"{digest}\"");
    assert_eq!(
        message(manifest(
            "1",
            "\"distro.erofs\"",
            &sha256,
            "[{\"name\": \"systemd\"}]"
        )),
        "systemd images are unsupported by the sandbox security profile"
    );
    // Python compares the format numerically, so 1.0 and true are 1.
    for format in ["1", "1.0", "true"] {
        let binding = manifest(
            format,
            "\"distro.erofs\"",
            &sha256,
            "[{\"name\": \"bash\"}]",
        )
        .unwrap();
        // The binding seals the manifest that the verification read.
        let sealed = crate::host::seal::FileSeal::of(
            &fs::File::open(layer.path("distro.erofs.manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(binding.manifest(), Some(&sealed));
    }
    for (format, artifact, sha256) in [
        ("\"1\"", "\"distro.erofs\"", sha256.as_str()),
        ("2", "\"distro.erofs\"", &sha256),
        ("1", "\"other.erofs\"", &sha256),
        ("1", "\"distro.erofs\"", "\"0\""),
    ] {
        assert_eq!(
            message(manifest(format, artifact, sha256, "[]")),
            "sandbox distro manifest does not match its artifact"
        );
    }
    for packages in [
        "{}",
        "[1]",
        "[{}]",
        "[{\"name\": 1}]",
        "[{\"name\": \"systemd\"}, 1]",
    ] {
        assert_eq!(
            message(manifest("1", "\"distro.erofs\"", &sha256, packages)),
            "sandbox distro manifest has invalid package metadata"
        );
    }
    layer.manifest("[]");
    assert_eq!(
        message(validate_sandbox_systemd_policy(&layer.launch("/sbin/init"))),
        "invalid sandbox distro manifest: expected a JSON object"
    );
    layer.manifest("{\r\n\"format\": }");
    assert_eq!(
        message(validate_sandbox_systemd_policy(&layer.launch("/sbin/init"))),
        "invalid sandbox distro manifest: Expecting value: line 2 column 11 (char 12)"
    );
    fs::write(layer.path("distro.erofs.manifest.json"), b"{\xff}").unwrap();
    assert_eq!(
        message(validate_sandbox_systemd_policy(&layer.launch("/sbin/init"))),
        "'utf-8' codec can't decode byte 0xff in position 1: invalid start byte"
    );
    // A layer without a manifest passes.
    fs::remove_file(layer.path("distro.erofs.manifest.json")).unwrap();
    validate_sandbox_systemd_policy(&layer.launch("/sbin/init")).unwrap();
}

#[test]
fn sandbox_managed_operations_reject_dry_run_before_state_access() {
    for operation in [
        SandboxOperation::Provision,
        SandboxOperation::Start,
        SandboxOperation::Exec,
        SandboxOperation::Stop,
        SandboxOperation::Deprovision,
    ] {
        let options = SandboxOptions {
            dry_run: true,
            ..SandboxOptions::new(operation)
        };
        assert_eq!(
            message(options.check()),
            "--dry-run is only valid for sandbox run"
        );
    }
    let run = SandboxOptions {
        dry_run: true,
        ..launching(SandboxOperation::Run)
    };
    run.check().unwrap();
}

#[test]
fn sandbox_exec_forwards_explicit_empty_environment_and_cwd() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("environment.json");
    fs::write(&file, "[]").unwrap();
    let options = SandboxOptions {
        cwd: Some("/work".to_owned()),
        environment_file: Some(file.to_str().unwrap().to_owned()),
        ..SandboxOptions::new(SandboxOperation::Exec)
    };
    assert_eq!(options.check().unwrap(), Some(Vec::new()));
}

#[test]
fn sandbox_exec_forwards_environment_inheritance() {
    for (environment, expected) in [
        (vec!["A=1"], Some(vec!["A=1"])),
        (vec!["A=1", "TERM=dumb"], Some(vec!["A=1", "TERM=dumb"])),
        // Without entries the workload gets the defaults whatever the flag says.
        (Vec::new(), None),
    ] {
        let options = SandboxOptions {
            environment: environment.iter().map(|&entry| entry.to_owned()).collect(),
            inherit_default_environment: true,
            ..SandboxOptions::new(SandboxOperation::Exec)
        };
        let expected =
            expected.map(|entries| entries.iter().map(|&entry| entry.to_owned()).collect());
        assert_eq!(options.check().unwrap(), expected);
    }
}

#[test]
fn sandbox_exec_rejects_ambiguous_or_one_shot_environment() {
    let both = SandboxOptions {
        environment: vec!["A=1".to_owned()],
        environment_file: Some("environment.json".to_owned()),
        ..SandboxOptions::new(SandboxOperation::Exec)
    };
    assert_eq!(
        message(both.check()),
        "--environment and --environment-file are mutually exclusive"
    );
    let one_shot = SandboxOptions {
        environment: vec!["A=1".to_owned()],
        ..SandboxOptions::new(SandboxOperation::Run)
    };
    let exec_only = "managed execution options require the sandbox exec operation";
    assert_eq!(message(one_shot.check()), exec_only);
    for operation in [SandboxOperation::Run, SandboxOperation::Start] {
        let inheriting = SandboxOptions {
            inherit_default_environment: true,
            ..SandboxOptions::new(operation)
        };
        assert_eq!(message(inheriting.check()), exec_only);
        let cwd = SandboxOptions {
            cwd: Some("/".to_owned()),
            ..SandboxOptions::new(operation)
        };
        assert_eq!(message(cwd.check()), exec_only);
    }
}

#[test]
fn sandbox_environment_file_is_bounded_before_state_access() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("environment.json");
    fs::write(&file, vec![b' '; MAX_ENVIRONMENT_FILE_BYTES + 1]).unwrap();
    assert_eq!(
        message(exec_environment_from_file(file.to_str().unwrap())),
        "managed execution environment file exceeds 1048576-byte limit"
    );
    // A file of the limit is read, and its spaces are no JSON value.
    fs::write(&file, vec![b' '; MAX_ENVIRONMENT_FILE_BYTES]).unwrap();
    let path = PurePath::native(file.to_str().unwrap());
    assert_eq!(
        message(exec_environment_from_file(file.to_str().unwrap())),
        format!("failed to read managed execution environment: {path}")
    );
}

#[test]
fn sandbox_environment_is_validated_before_state_access() {
    let too_many: Vec<String> = (0..257).map(|index| format!("KEY{index}=value")).collect();
    let entries = |entries: &[&str]| entries.iter().map(|&entry| entry.to_owned()).collect();
    let non_empty = "managed exec environment entries must be non-empty KEY=VALUE strings of at most 4096 bytes";
    for (environment, expected) in [
        (
            too_many,
            "managed exec environment exceeds 256 entries".to_owned(),
        ),
        (entries(&["NO_EQUALS"]), non_empty.to_owned()),
        (entries(&["=empty-key"]), non_empty.to_owned()),
        (entries(&["KEY=embedded\0nul"]), non_empty.to_owned()),
        (
            vec![format!("KEY={}", "\u{2603}".repeat(1365))],
            non_empty.to_owned(),
        ),
        (
            entries(&["DUPLICATE=one", "DUPLICATE=two"]),
            "managed exec environment contains duplicate key: DUPLICATE".to_owned(),
        ),
    ] {
        let options = SandboxOptions {
            environment,
            ..SandboxOptions::new(SandboxOperation::Exec)
        };
        assert_eq!(message(options.check()), expected);
    }
    // An entry of exactly 4096 bytes fits.
    validate_exec_environment(&[format!("K={}", "v".repeat(4094))]).unwrap();
}

#[test]
fn sandbox_environment_file_entries_are_validated_before_state_access() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("environment.json");
    let read = |text: &str| {
        fs::write(&file, text).unwrap();
        exec_environment_from_file(file.to_str().unwrap())
    };
    assert_eq!(
        message(read(r#"["DUPLICATE=one", "DUPLICATE=two"]"#)),
        "managed exec environment contains duplicate key: DUPLICATE"
    );
    assert_eq!(
        read(r#"["A=1", "B=\u00e9"]"#).unwrap(),
        ["A=1".to_owned(), "B=\u{e9}".to_owned()]
    );
    let not_strings =
        "managed execution environment file must contain a JSON array of KEY=VALUE strings";
    for text in ["{}", "\"A=1\"", "[1]", "[NaN]", "[[\"A=1\"]]"] {
        assert_eq!(message(read(text)), not_strings, "{text}");
    }
    let path = PurePath::native(file.to_str().unwrap());
    let unreadable = format!("failed to read managed execution environment: {path}");
    for text in ["", "[", "[\"A=1\",]", "\u{feff}[]", "[\"a\tb\"]"] {
        assert_eq!(message(read(text)), unreadable, "{text:?}");
    }
    fs::write(&file, b"[\"\xff\"]").unwrap();
    assert_eq!(
        message(exec_environment_from_file(file.to_str().unwrap())),
        unreadable
    );
    // Python refuses to encode unpaired surrogates, after it accepted them in the file.
    assert_eq!(
        message(read(r#"["A=1", "B=\ud800"]"#)),
        "'utf-8' codec can't encode character '\\ud800' in position 2: surrogates not allowed"
    );
    assert_eq!(
        message(read(r#"["B=\udc00\ud800x"]"#)),
        "'utf-8' codec can't encode characters in position 2-3: surrogates not allowed"
    );
    // `int` refuses an integer of more than 4300 digits.
    assert_eq!(
        message(read(&format!("[{}]", "1".repeat(4301)))),
        "Exceeds the limit (4300 digits) for integer string conversion: value has 4301 digits; \
         use sys.set_int_max_str_digits() to increase the limit"
    );
    assert_eq!(
        message(exec_environment_from_file(
            directory.path().join("missing.json").to_str().unwrap()
        )),
        format!(
            "failed to read managed execution environment: {}",
            PurePath::native(directory.path().join("missing.json").to_str().unwrap())
        )
    );
}

#[test]
fn managed_non_launch_operations_reject_network_policy_options() {
    for operation in [
        SandboxOperation::Start,
        SandboxOperation::Exec,
        SandboxOperation::Stop,
        SandboxOperation::Deprovision,
    ] {
        let options = SandboxOptions {
            network_options: true,
            ..SandboxOptions::new(operation)
        };
        assert_eq!(
            message(options.check()),
            "network policy options are only valid for sandbox run or provision"
        );
    }
    for operation in [SandboxOperation::Run, SandboxOperation::Provision] {
        let options = SandboxOptions {
            network_options: true,
            net: true,
            ..launching(operation)
        };
        assert_eq!(
            message(options.check()),
            "--net and --network-profile must be specified together"
        );
        let profile = SandboxOptions {
            network_options: true,
            network_profile: true,
            ..launching(operation)
        };
        assert_eq!(
            message(profile.check()),
            "--net and --network-profile must be specified together"
        );
        let missing = SandboxOptions {
            scratch: false,
            ..launching(operation)
        };
        assert_eq!(
            message(missing.check()),
            format!("sandbox {operation} requires --layer and --scratch")
        );
    }
    let report = SandboxOptions {
        outcome_report: true,
        ..SandboxOptions::new(SandboxOperation::Start)
    };
    assert_eq!(
        message(report.check()),
        "--outcome-report is only valid for one-shot run or managed exec"
    );
}

#[test]
fn sandbox_command_rejects_misplaced_mount_options() {
    for kind in POLICY_KINDS {
        let options = SandboxOptions {
            policy_paths: vec![PolicyPathOption {
                kind,
                preceding_mount: None,
                path: "secrets".to_owned(),
            }],
            ..launching(SandboxOperation::Run)
        };
        assert_eq!(
            message(options.check()),
            format!("{} requires --mount", kind.option())
        );
    }
    let owner = SandboxOptions {
        mount_owner: Some(MountOwner::Caller),
        ..launching(SandboxOperation::Run)
    };
    assert_eq!(message(owner.check()), "--mount-owner requires --mount");
    for operation in [
        SandboxOperation::Start,
        SandboxOperation::Exec,
        SandboxOperation::Stop,
        SandboxOperation::Deprovision,
    ] {
        let options = SandboxOptions {
            mounts: vec!["/workspace,share,rw".to_owned()],
            ..SandboxOptions::new(operation)
        };
        assert_eq!(
            message(options.check()),
            "--mount is only valid for sandbox run or provision"
        );
    }
}

/// Host directories for shares in a temporary directory, with a distro layer and a scratch
/// image.
struct Shares {
    /// The temporary directory.
    directory: tempfile::TempDir,
    /// The directory, resolved.
    root: PurePath,
}

impl Shares {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = resolve(&PurePath::native(directory.path().to_str().unwrap())).unwrap();
        fs::write(root.join("distro.erofs").as_path(), b"distro").unwrap();
        fs::write(root.join("scratch.ext4").as_path(), b"scratch").unwrap();
        Self { directory, root }
    }

    fn path(&self, name: &str) -> String {
        let _ = &self.directory;
        self.root.join(name).as_str().to_owned()
    }

    fn directory(&self, name: &str) -> String {
        let path = self.path(name);
        fs::create_dir_all(&path).unwrap();
        path
    }

    /// Runs the steps of `nvx.py sandbox --dry-run` that build the OpenVMM arguments and the
    /// kernel command line of these shares.
    fn run(
        &self,
        mounts: &[String],
        policy_paths: &[(PolicyKind, Option<usize>, &str)],
        owner: Option<MountOwner>,
    ) -> Result<(Vec<String>, String)> {
        let mounts = mounts.to_vec();
        let policy_paths: Vec<PolicyPathOption> = policy_paths
            .iter()
            .map(|&(kind, preceding_mount, path)| PolicyPathOption {
                kind,
                preceding_mount,
                path: path.to_owned(),
            })
            .collect();
        SandboxOptions {
            mounts: mounts.clone(),
            policy_paths: policy_paths.clone(),
            mount_owner: owner,
            dry_run: true,
            ..launching(SandboxOperation::Run)
        }
        .check()?;
        let launch = SandboxLaunch::new(
            vec![SandboxLayer::new(
                LayerRole::Distro,
                &self.path("distro.erofs"),
                "11111111-1111-1111-1111-111111111111",
            )],
            &self.path("scratch.ext4"),
            SandboxSettings::default(),
        )?;
        let mounts = sandbox_mounts(&mounts, &policy_paths, owner)?;
        validate_mounts(&mounts)?;
        launch.validated()?;
        let launch = launch.with_export(plan_mounts(&mounts)?)?;
        validate_sandbox_systemd_policy(&launch)?;
        // The command runs one-shot, which renders the shares as boot tokens.
        Ok((
            launch.plan().one_shot_openvmm_arguments()?,
            launch.plan().one_shot_kernel_command_line("")?,
        ))
    }
}

/// Returns the values of `option` in `arguments`.
fn values(arguments: &[String], option: &str) -> Vec<String> {
    arguments
        .windows(2)
        .filter(|pair| pair[0] == option)
        .map(|pair| pair[1].clone())
        .collect()
}

#[test]
fn sandbox_command_forwards_mount_to_openvmm() {
    let shares = Shares::new();
    let share = shares.directory("share");
    let secrets = shares.directory("share/secrets");
    let (arguments, _) = shares
        .run(
            &[format!("/workspace,{share},rw")],
            &[(PolicyKind::Denied, Some(0), "secrets")],
            None,
        )
        .unwrap();
    assert_eq!(
        values(&arguments, "--mount"),
        [format!("/workspace,{share},rw")]
    );
    // The plan resolves each policy path, which must exist.
    assert_eq!(values(&arguments, "--mount-deny"), [secrets]);
    assert!(!arguments.contains(&"--mount-owner".to_owned()));
    fs::remove_dir(shares.path("share/secrets")).unwrap();
    let error = shares
        .run(
            &[format!("/workspace,{share},rw")],
            &[(PolicyKind::Denied, Some(0), "secrets")],
            None,
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("does not exist, so it cannot be hidden"),
        "{error}"
    );
}

#[test]
fn sandbox_command_shares_a_file() {
    let shares = Shares::new();
    let settings = shares.path("settings.json");
    fs::write(&settings, b"{}").unwrap();
    let (arguments, command_line) = shares
        .run(
            &[format!("/etc/app/settings.json,{settings},ro")],
            &[],
            None,
        )
        .unwrap();
    let name = aggregate_child_name(0, "/etc/app/settings.json");
    // The regular file is a child of its own, without its directory, which OpenVMM must find to
    // be a regular file.
    assert!(!arguments.contains(&"--mount".to_owned()));
    assert_eq!(
        values(&arguments, "--mount-child"),
        [format!("{name},{settings},ro,file")]
    );
    assert!(command_line.contains(&format!("nvx_share={name},/etc/app/settings.json,ro,file")));
    let error = shares
        .run(
            &[format!("/etc/app/settings.json,{settings},rw")],
            &[(PolicyKind::Writable, Some(0), &settings)],
            None,
        )
        .unwrap_err();
    assert!(error.to_string().contains("shares a file"), "{error}");
}

#[test]
fn sandbox_command_shares_host_paths_with_commas() {
    let shares = Shares::new();
    let alpha = shares.directory("alpha,beta");
    let reports = shares.directory("2026, final");
    shares.directory("alpha,beta/secret");
    shares.directory("2026, final/secret");
    let (arguments, _) = shares
        .run(
            &[format!("/work,{alpha},rw")],
            &[(PolicyKind::Denied, Some(0), "secret")],
            None,
        )
        .unwrap();
    assert_eq!(values(&arguments, "--mount"), [format!("/work,{alpha},rw")]);
    assert_eq!(
        values(&arguments, "--mount-deny"),
        [shares.path("alpha,beta/secret")]
    );
    let (arguments, _) = shares
        .run(
            &[
                format!("/work,{alpha},rw"),
                format!("/reports,{reports},ro"),
            ],
            &[
                (PolicyKind::Denied, Some(0), "secret"),
                (PolicyKind::Denied, Some(1), "secret"),
            ],
            None,
        )
        .unwrap();
    // The children follow their resolved host paths.
    let (report, work) = (
        aggregate_child_name(0, "/reports"),
        aggregate_child_name(1, "/work"),
    );
    let pairs: Vec<(String, String)> = arguments
        .windows(2)
        .filter(|pair| pair[0] == "--mount-child" || pair[0] == "--mount-deny")
        .map(|pair| (pair[0].clone(), pair[1].clone()))
        .collect();
    assert_eq!(
        pairs,
        [
            ("--mount-child".to_owned(), format!("{report},{reports},ro")),
            ("--mount-child".to_owned(), format!("{work},{alpha},rw")),
            (
                "--mount-deny".to_owned(),
                shares.root.join("2026, final/secret").as_str().to_owned()
            ),
            (
                "--mount-deny".to_owned(),
                shares.root.join("alpha,beta/secret").as_str().to_owned()
            ),
        ]
    );
    let error = shares
        .run(&[format!("/work,{alpha}")], &[], None)
        .unwrap_err();
    assert!(
        error.to_string().contains("end the value with ,ro or ,rw"),
        "{error}"
    );
}

#[test]
fn sandbox_command_forwards_mount_owner() {
    let shares = Shares::new();
    let share = shares.directory("share");
    let result = shares.run(
        &[format!("/workspace,{share},rw")],
        &[],
        Some(MountOwner::Caller),
    );
    if cfg!(windows) {
        assert!(message(result).contains("Linux host"));
        return;
    }
    let owner = crate::host::python::fs::stat(Path::new(&share)).unwrap();
    if owner.uid == 0 || owner.gid == 0 {
        // Caller ownership refuses a root-owned share.
        return;
    }
    let (arguments, _) = result.unwrap();
    assert_eq!(values(&arguments, "--mount-owner"), ["caller"]);
    let position = |option: &str| arguments.iter().position(|argument| argument == option);
    assert!(position("--mount") < position("--mount-owner"));
}

#[test]
fn sandbox_command_attributes_policy_paths_to_the_preceding_mount() {
    let shares = Shares::new();
    let work = shares.directory("work");
    let tools = shares.directory("tools");
    for name in ["work/logs/payloads", "work/out", "tools/credentials"] {
        shares.directory(name);
    }
    let mounts = [
        format!("/workspace,{work},rw"),
        format!("/opt/hostedtoolcache,{tools},ro"),
    ];
    let policy = [
        (PolicyKind::Denied, Some(0), "logs"),
        (PolicyKind::Allowed, Some(0), "logs/payloads"),
        (PolicyKind::Writable, Some(0), "out"),
        (PolicyKind::Denied, Some(1), "credentials"),
    ];
    let options: Vec<PolicyPathOption> = policy
        .iter()
        .map(|&(kind, preceding_mount, path)| PolicyPathOption {
            kind,
            preceding_mount,
            path: path.to_owned(),
        })
        .collect();
    let parsed = sandbox_mounts(&mounts, &options, None).unwrap();
    assert_eq!(parsed[0].policy().denied, ["logs"]);
    assert_eq!(parsed[0].policy().allowed, ["logs/payloads"]);
    assert_eq!(parsed[0].policy().writable, ["out"]);
    assert_eq!(parsed[1].policy().denied, ["credentials"]);
    assert!(parsed[1].policy().allowed.is_empty() && parsed[1].policy().writable.is_empty());
    let (arguments, _) = shares.run(&mounts, &policy, None).unwrap();
    // With several shares, OpenVMM attributes absolute paths.
    assert_eq!(
        values(&arguments, "--mount-allow"),
        [shares.root.join("work/logs/payloads").as_str()]
    );
    assert_eq!(
        values(&arguments, "--mount-write"),
        [shares.root.join("work/out").as_str()]
    );

    let workspace = format!("/workspace,{work},rw");
    let toolcache = format!("/opt/hostedtoolcache,{tools}");
    let cache = shares.root.join("tools/cache").as_str().to_owned();
    let error = |mounts: &[String], policy: &[(PolicyKind, Option<usize>, &str)]| {
        shares.run(mounts, policy, None).unwrap_err().to_string()
    };
    assert_eq!(
        error(&[], &[(PolicyKind::Writable, None, "out")]),
        "--mount-write requires --mount"
    );
    // A --mount-write after the read-only share would narrow it.
    assert!(
        error(
            &[workspace.clone(), format!("{toolcache},ro")],
            &[(PolicyKind::Writable, Some(1), "cache")]
        )
        .contains("is read-only")
    );
    assert!(
        error(
            &[workspace.clone(), toolcache.clone()],
            &[(PolicyKind::Allowed, None, "logs")]
        )
        .contains("each --mount-allow must follow the --mount")
    );
    // A path in one share cannot be attributed to the other.
    assert_eq!(
        error(
            &[workspace.clone(), toolcache.clone()],
            &[(PolicyKind::Writable, Some(0), &cache)]
        ),
        format!(
            "--mount-write {cache} is not inside the host directory of --mount /workspace; with \
             several shares, each --mount-write follows the --mount whose directory it names"
        )
    );
    // Only a denied path may name a share's root.
    assert!(
        error(
            &[workspace.clone(), toolcache.clone()],
            &[
                (PolicyKind::Denied, Some(0), "logs"),
                (PolicyKind::Allowed, Some(0), &work)
            ]
        )
        .contains(&format!(
            "--mount-allow {work} is not inside the host directory"
        ))
    );
    assert!(
        error(
            &[workspace.clone(), toolcache.clone()],
            &[(PolicyKind::Writable, Some(0), &work)]
        )
        .contains(&format!(
            "--mount-write {work} is not inside the host directory"
        ))
    );
}

#[test]
fn sandbox_command_hides_a_share_root_behind_its_allowed_paths() {
    let shares = Shares::new();
    let work = shares.directory("work");
    let src = shares.directory("work/src");
    let tools = shares.directory("tools");
    let (arguments, _) = shares
        .run(
            &[
                format!("/workspace,{work},rw"),
                format!("/opt/hostedtoolcache,{tools}"),
            ],
            &[
                (PolicyKind::Denied, Some(0), &work),
                (PolicyKind::Allowed, Some(0), &src),
            ],
            None,
        )
        .unwrap();
    // OpenVMM hides the workspace's root behind its one allowed path.
    let (toolcache, workspace) = (
        aggregate_child_name(0, "/opt/hostedtoolcache"),
        aggregate_child_name(1, "/workspace"),
    );
    assert_eq!(
        values(&arguments, "--mount-child"),
        [
            format!("{toolcache},{tools},ro"),
            format!("{workspace},{work},rw")
        ]
    );
    assert_eq!(values(&arguments, "--mount-deny"), [work]);
    assert_eq!(values(&arguments, "--mount-allow"), [src]);
}

#[test]
fn sandbox_command_attributes_mount_deny_to_the_preceding_mount() {
    let shares = Shares::new();
    let work = shares.directory("work");
    let tools = shares.directory("tools");
    for name in ["work/secrets", "tools/credentials", "tools/logs"] {
        shares.directory(name);
    }
    let mounts = [
        format!("/workspace,{work},rw"),
        format!("/opt/hostedtoolcache,{tools},ro"),
    ];
    let policy = [
        (PolicyKind::Denied, Some(0), "secrets"),
        (PolicyKind::Denied, Some(1), "credentials"),
        (PolicyKind::Denied, Some(1), "logs"),
    ];
    let (arguments, command_line) = shares.run(&mounts, &policy, None).unwrap();
    // The children follow their resolved host paths, each with its own denied paths.
    let (toolcache, workspace) = (
        aggregate_child_name(0, "/opt/hostedtoolcache"),
        aggregate_child_name(1, "/workspace"),
    );
    assert!(!arguments.contains(&"--mount".to_owned()));
    assert_eq!(values(&arguments, "--mount-aggregate"), ["/run/nvx/shares"]);
    assert_eq!(
        values(&arguments, "--mount-child"),
        [
            format!("{toolcache},{tools},ro"),
            format!("{workspace},{work},rw")
        ]
    );
    assert_eq!(
        values(&arguments, "--mount-deny"),
        [
            shares.root.join("tools/credentials").as_str(),
            shares.root.join("tools/logs").as_str(),
            shares.root.join("work/secrets").as_str(),
        ]
    );
    assert!(command_line.contains(&format!(
        "nvx_share={toolcache},/opt/hostedtoolcache,ro nvx_share={workspace},/workspace,rw"
    )));

    // With one share, a --mount-deny may come first; with several, it must follow the share
    // whose directory it hides.
    let single = sandbox_mounts(
        &[format!("/w,{work},rw")],
        &[PolicyPathOption {
            kind: PolicyKind::Denied,
            preceding_mount: None,
            path: "secrets".to_owned(),
        }],
        None,
    )
    .unwrap();
    assert_eq!(single[0].policy().denied, ["secrets"]);
    let error = shares
        .run(
            &[
                format!("/workspace,{work},rw"),
                format!("/opt/hostedtoolcache,{tools}"),
            ],
            &[(PolicyKind::Denied, None, "secrets")],
            None,
        )
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "with several --mount options, each --mount-deny must follow the --mount whose host \
         directory it names"
    );
    // More shares are children of the same aggregate.
    let cache = shares.directory("cache");
    let (arguments, _) = shares
        .run(
            &[
                format!("/a,{work}"),
                format!("/b,{tools}"),
                format!("/c,{cache}"),
            ],
            &[],
            None,
        )
        .unwrap();
    assert_eq!(values(&arguments, "--mount-child").len(), 3);
}

#[test]
fn sandbox_mounts_take_the_kind_of_each_host_path() {
    let mounts = ["/a,a".to_owned(), "/b,b.json,rw".to_owned()];
    let kind_of = |mount: &SandboxMount| {
        if mount.guest_target() == "/b" {
            MountKind::File
        } else {
            MountKind::Directory
        }
    };
    let parsed = sandbox_mounts_with(
        Flavor::Posix,
        &mounts,
        &[],
        Some(MountOwner::Caller),
        &kind_of,
    )
    .unwrap();
    assert_eq!(parsed[0].kind(), MountKind::Directory);
    assert_eq!(parsed[1].kind(), MountKind::File);
    assert_eq!(parsed[0].owner(), MountOwner::Caller);
    assert_eq!(parsed[1].owner(), MountOwner::Caller);
}

#[test]
fn run_share_targets_bind_from_an_aggregate() {
    validate_run_share_targets(&["/workspace", "/opt/hostedtoolcache", "/srv/cache"]).unwrap();
    validate_run_share_targets(&["/config/settings.json"]).unwrap();
    for (targets, expected) in [
        (
            &["/workspace", "/workspace/cache"][..],
            "--mount targets /workspace and /workspace/cache overlap; a share cannot hide another",
        ),
        (
            &["/run/nvx", "/b"],
            "--mount targets /run/nvx/shares and /run/nvx overlap; a share cannot hide another",
        ),
        (
            &["/run/nvx/shares/x"],
            "--mount targets /run/nvx/shares and /run/nvx/shares/x overlap; a share cannot hide \
             another",
        ),
    ] {
        assert_eq!(message(validate_run_share_targets(targets)), expected);
    }
    for target in [
        "/a b", "relative", "/", "/a//b", "/a/./b", "/a\\b", "/a=b", "/a,b",
    ] {
        assert_eq!(
            message(validate_run_share_targets(&[target, "/b"])),
            format!(
                "invalid --mount target {}: with several shares, or a shared file, each target \
                 must be an absolute non-root path without empty, dot, or parent components, \
                 whitespace, '\\', '=', or ','",
                repr(target)
            )
        );
    }
    // Unlike a sandbox target, a run target has no length limit.
    validate_run_share_targets(&[&format!("/{}", "x".repeat(5000))]).unwrap();
}
