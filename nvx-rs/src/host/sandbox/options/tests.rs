//! The `nvx.py sandbox` and `nvx.py run` option tests of `scripts/test_nvx_tools.py`, ported.

use std::fs;
use std::path::{Path, PathBuf};

use proptest::prelude::*;
#[cfg(feature = "cli")]
use serde_json::json;
#[cfg(feature = "cli")]
use sha2::{Digest, Sha256};

use super::*;
use crate::ErrorKind;
use crate::host::paths::{self, resolve};
use crate::host::sandbox::DEFAULT_ENTRYPOINT;
#[cfg(feature = "cli")]
use crate::host::sandbox::distro::{DISTRO_POLICY_REVISION, DistroBinding, DistroFormat};
#[cfg(feature = "cli")]
use crate::host::sandbox::layer::{LayerRole, SandboxLayer};
use crate::host::sandbox::mount::{MountAccess, plan_mounts, validate_mounts};
use crate::host::sandbox::tests::policy as mount_policy;

fn message<T: std::fmt::Debug>(result: Result<T>) -> String {
    let error = result.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
    error.to_string()
}

/// Returns the options of `operation` with the defaults of `nvx-rs sandbox`.
fn defaults(operation: SandboxOperation) -> SandboxOptions {
    SandboxOptions {
        operation,
        dry_run: false,
        cwd: None,
        environment: Vec::new(),
        environment_file: None,
        inherit_default_environment: false,
        network_options: false,
        net: false,
        network_profile: false,
        entrypoint: DEFAULT_ENTRYPOINT.to_owned(),
        outcome_report: false,
        mounts: Vec::new(),
        policy_paths: Vec::new(),
        mount_owner: None,
        layers: false,
        scratch: false,
        upper: None,
    }
}

fn launching(operation: SandboxOperation) -> SandboxOptions {
    SandboxOptions {
        layers: true,
        scratch: true,
        ..defaults(operation)
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
            ..defaults(SandboxOperation::Stop)
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
#[cfg(feature = "cli")]
struct Layer {
    /// The temporary directory.
    directory: tempfile::TempDir,
}

#[cfg(feature = "cli")]
impl Layer {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("distro.erofs"), b"distro").unwrap();
        Self { directory }
    }

    fn path(&self, name: &str) -> String {
        let path = paths::normalized(&self.directory.path().join(name));
        path.to_str().unwrap().to_owned()
    }

    fn layer(&self) -> SandboxLayer {
        SandboxLayer::new(
            LayerRole::Distro,
            &self.path("distro.erofs"),
            "11111111-1111-1111-1111-111111111111",
        )
    }

    /// Refuses a systemd `entrypoint`, and then verifies the layer, as the preflight of
    /// `nvx-rs sandbox run` and `provision` does.
    fn preflight(&self, entrypoint: &str) -> Result<DistroBinding> {
        validate_sandbox_entrypoint(entrypoint)?;
        DistroBinding::verify(&self.layer())
    }

    fn manifest(&self, text: &str) {
        fs::write(self.path("distro.erofs.manifest.json"), text).unwrap();
    }
}

#[cfg(feature = "cli")]
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

#[cfg(feature = "cli")]
#[test]
fn distro_manifest_binds_the_layer() {
    let layer = Layer::new();
    let valid = json!({
        "format": 1,
        "artifact": "distro.erofs",
        "artifact_sha256": distro_sha256(),
        "packages": [{"name": "bash"}, {"name": "coreutils"}],
    });
    layer.manifest(&valid.to_string());
    let binding = layer.preflight("/bin/sh").unwrap();
    let recorded = serde_json::to_value(&binding).unwrap();
    assert_eq!(recorded["identity"], "11111111-1111-1111-1111-111111111111");
    assert_eq!(recorded["format"], DistroFormat::Erofs.as_str());
    assert!(recorded["manifest"].is_object());
    binding
        .check(&layer.layer(), DISTRO_POLICY_REVISION)
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
            layer.preflight("/bin/sh").unwrap_err().kind(),
            ErrorKind::Unsupported,
            "{field}: {manifest}",
        );
    }
    for field in ["format", "artifact", "artifact_sha256", "packages"] {
        let mut manifest = valid.clone();
        manifest.as_object_mut().unwrap().remove(field);
        layer.manifest(&manifest.to_string());
        assert_eq!(
            layer.preflight("/bin/sh").unwrap_err().kind(),
            ErrorKind::Unsupported,
        );
    }
    layer.manifest(&valid.to_string());
    fs::write(layer.path("distro.erofs"), b"changed distro").unwrap();
    assert_eq!(
        layer.preflight("/bin/sh").unwrap_err().kind(),
        ErrorKind::Unsupported,
    );
}

#[cfg(feature = "cli")]
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
                layer.preflight(entrypoint).unwrap_err().kind(),
                ErrorKind::Unsupported,
            );
        }
        let result = layer.preflight("/sbin/init");
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
    let denied = mount_policy(&["secret"], &[], &[]);
    assert_eq!(
        mounts,
        [SandboxMount::parse(&first, denied, MountOwner::Vmm, MountKind::Directory).unwrap()]
    );
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
            let share = SandboxMount::new(
                &format!("/share-{index}"), &format!("host-{index}"), MountAccess::ReadWrite,
                policy, MountOwner::Vmm, MountKind::Directory,
            ).unwrap();
            prop_assert_eq!(mount, &share);
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

#[cfg(feature = "cli")]
#[test]
fn sandbox_allows_non_systemd_sbin_init() {
    let layer = Layer::new();
    layer.preflight("/sbin/init").unwrap();
    assert_eq!(
        message(layer.preflight("/lib/systemd/systemd")),
        "systemd entrypoints are unsupported by the sandbox security profile"
    );
}

#[cfg(feature = "cli")]
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
    let binding = layer.preflight("/sbin/init").unwrap();
    assert!(serde_json::to_value(&binding).unwrap()["manifest"].is_null());
    assert_eq!(
        message(layer.preflight("/usr/lib/systemd/systemd")),
        "systemd entrypoints are unsupported by the sandbox security profile"
    );
    // The entrypoint check precedes the manifest, which it never reads.
    layer.manifest("not JSON");
    assert_eq!(
        message(layer.preflight("/lib/systemd/systemd")),
        "systemd entrypoints are unsupported by the sandbox security profile"
    );
}

#[cfg(feature = "cli")]
#[test]
fn sandbox_rejects_systemd_from_bound_distro_metadata() {
    let layer = Layer::new();
    let digest = distro_sha256();
    let manifest = |format: &str, artifact: &str, sha256: &str, packages: &str| {
        layer.manifest(&format!(
            "{{\"format\": {format}, \"artifact\": {artifact}, \"artifact_sha256\": {sha256}, \
             \"packages\": {packages}}}"
        ));
        layer.preflight("/sbin/init")
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
    let binding = manifest("1", "\"distro.erofs\"", &sha256, "[{\"name\": \"bash\"}]").unwrap();
    let sealed = crate::host::seal::FileSeal::of(
        &fs::File::open(layer.path("distro.erofs.manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(&binding).unwrap()["manifest"],
        serde_json::to_value(&sealed).unwrap()
    );
    for (format, artifact, sha256) in [
        ("\"1\"", "\"distro.erofs\"", sha256.as_str()),
        ("1.0", "\"distro.erofs\"", &sha256),
        ("true", "\"distro.erofs\"", &sha256),
        // An object, which `serde_json`'s own `Value` would read as the number 1 with the
        // `arbitrary_precision` feature that another crate of a build may enable.
        (
            "{\"$serde_json::private::Number\": \"1\"}",
            "\"distro.erofs\"",
            &sha256,
        ),
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
        message(layer.preflight("/sbin/init")),
        "invalid sandbox distro manifest: expected a JSON object"
    );
    layer.manifest("{\r\n\"format\": }");
    let error = message(layer.preflight("/sbin/init"));
    assert!(error.starts_with("invalid sandbox distro manifest: "));
    assert!(error.contains("line 2 column 11"), "{error}");
    fs::write(layer.path("distro.erofs.manifest.json"), b"{\xff}").unwrap();
    let error = message(layer.preflight("/sbin/init"));
    assert!(
        error.starts_with("invalid sandbox distro manifest: "),
        "{error}"
    );
    // A layer without a manifest passes.
    fs::remove_file(layer.path("distro.erofs.manifest.json")).unwrap();
    layer.preflight("/sbin/init").unwrap();
}

#[cfg(feature = "cli")]
#[test]
fn distro_manifests_are_strict_json() {
    let layer = Layer::new();
    let digest = distro_sha256();
    let with_extra = |extra: &str| {
        layer.manifest(&format!(
            r#"{{"format":1,"artifact":"distro.erofs","artifact_sha256":"{digest}","packages":[],"extra":{extra}}}"#
        ));
        message(layer.preflight("/bin/sh"))
    };
    for extra in [
        "NaN",
        "Infinity",
        "-Infinity",
        "1e400",
        r#""\ud800""#,
        r#""\udc00""#,
    ] {
        let error = with_extra(extra);
        assert!(
            error.starts_with("invalid sandbox distro manifest: "),
            "{error}"
        );
        assert!(error.contains("line "), "{error}");
    }
    // The manifest is read as egress policy files are: an object repeats no field, and arrays
    // and objects nest at most `MAX_JSON_DEPTH` deep, the root object included.
    let nested = |depth: usize| format!("{}{}", "[".repeat(depth), "]".repeat(depth));
    layer.manifest(&format!(
        r#"{{"format":1,"artifact":"distro.erofs","artifact_sha256":"{digest}","packages":[],"extra":{}}}"#,
        nested(crate::host::network::policy::MAX_JSON_DEPTH - 1)
    ));
    layer.preflight("/bin/sh").unwrap();
    let error = with_extra(&nested(crate::host::network::policy::MAX_JSON_DEPTH));
    assert!(
        error.starts_with(
            "invalid sandbox distro manifest: arrays and objects nest more than 32 deep at line 1"
        ),
        "{error}"
    );
    layer.manifest(&format!(
        r#"{{"format":1,"format":1,"artifact":"distro.erofs","artifact_sha256":"{digest}","packages":[]}}"#
    ));
    assert_eq!(
        message(layer.preflight("/bin/sh")),
        "invalid sandbox distro manifest: manifest has duplicate field \"format\""
    );
    layer.manifest(&format!(
        r#"{{"format":1,"artifact":"distro.erofs","artifact_sha256":"{digest}","packages":[{{"name":"bash","name":"systemd"}}]}}"#
    ));
    assert_eq!(
        message(layer.preflight("/bin/sh")),
        "invalid sandbox distro manifest: packages[0] has duplicate field \"name\""
    );
    fs::write(
        layer.path("distro.erofs.manifest.json"),
        b"{\"format\":\xff}",
    )
    .unwrap();
    let error = message(layer.preflight("/bin/sh"));
    assert!(
        error.starts_with("invalid sandbox distro manifest: not UTF-8: "),
        "{error}"
    );
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
            ..defaults(operation)
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
        ..defaults(SandboxOperation::Exec)
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
            ..defaults(SandboxOperation::Exec)
        };
        let expected =
            expected.map(|entries| entries.iter().map(|&entry| entry.to_owned()).collect());
        assert_eq!(options.check().unwrap(), expected);
    }
}

#[test]
fn sandbox_run_and_exec_alone_take_the_execution_options() {
    let both = SandboxOptions {
        environment: vec!["A=1".to_owned()],
        environment_file: Some("environment.json".to_owned()),
        ..defaults(SandboxOperation::Exec)
    };
    assert_eq!(
        message(both.check()),
        "--environment and --environment-file are mutually exclusive"
    );
    // A run executes its workload as exec does.
    let run = SandboxOptions {
        environment: vec!["A=1".to_owned()],
        cwd: Some("/tmp".to_owned()),
        inherit_default_environment: true,
        ..launching(SandboxOperation::Run)
    };
    assert_eq!(run.check().unwrap(), Some(vec!["A=1".to_owned()]));
    let exec_only = "managed execution options are only valid for sandbox run or exec";
    for operation in [
        SandboxOperation::Provision,
        SandboxOperation::Start,
        SandboxOperation::Stop,
        SandboxOperation::Deprovision,
    ] {
        let environment = SandboxOptions {
            environment: vec!["A=1".to_owned()],
            ..defaults(operation)
        };
        assert_eq!(message(environment.check()), exec_only);
        let inheriting = SandboxOptions {
            inherit_default_environment: true,
            ..defaults(operation)
        };
        assert_eq!(message(inheriting.check()), exec_only);
        let cwd = SandboxOptions {
            cwd: Some("/".to_owned()),
            ..defaults(operation)
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
    let error = message(exec_environment_from_file(file.to_str().unwrap()));
    assert!(error.starts_with(&format!("invalid managed execution environment {file:?}: ")));
    assert!(error.contains("EOF"), "{error}");
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
            ..defaults(SandboxOperation::Exec)
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
    let prefix = format!("invalid managed execution environment {file:?}: ");
    for text in [
        "{}",
        "\"A=1\"",
        "[1]",
        "[NaN]",
        "[[\"A=1\"]]",
        "",
        "[",
        "[\"A=1\",]",
        "\u{feff}[]",
        "[\"a\tb\"]",
        r#"["A=1", "B=\ud800"]"#,
        r#"["B=\udc00\ud800x"]"#,
        &format!("[{}]", "1".repeat(4301)),
    ] {
        let error = message(read(text));
        assert!(error.starts_with(&prefix), "{text:?}: {error}");
        assert!(error.contains("line "), "{error}");
    }
    fs::write(&file, b"[\"\xff\"]").unwrap();
    let error = message(exec_environment_from_file(file.to_str().unwrap()));
    assert!(error.starts_with(&prefix), "{error}");
    let missing = directory.path().join("missing.json");
    let error = message(exec_environment_from_file(missing.to_str().unwrap()));
    let cause = error
        .strip_prefix(&format!(
            "failed to read managed execution environment {missing:?}: "
        ))
        .unwrap();
    assert!(!cause.is_empty());
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
            ..defaults(operation)
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
        ..defaults(SandboxOperation::Start)
    };
    assert_eq!(
        message(report.check()),
        "--outcome-report is only valid for sandbox run or exec"
    );
}

#[test]
fn sandbox_launches_take_the_scratch_image_that_their_upper_needs() {
    for operation in [SandboxOperation::Run, SandboxOperation::Provision] {
        // Without --upper, as with --upper scratch, the upper is on the image of --scratch.
        for upper in [None, Some(RootUpper::Scratch)] {
            let options = SandboxOptions {
                upper,
                ..launching(operation)
            };
            assert_eq!(options.check().unwrap(), None);
            let missing = SandboxOptions {
                scratch: false,
                ..options
            };
            assert_eq!(
                message(missing.check()),
                format!("sandbox {operation} requires --layer and --scratch")
            );
        }
        // An upper in RAM has no scratch image, and needs the distro layer alone.
        let ram = SandboxOptions {
            upper: Some(RootUpper::Ram),
            scratch: false,
            ..launching(operation)
        };
        assert_eq!(ram.check().unwrap(), None);
        let with_scratch = SandboxOptions {
            scratch: true,
            ..ram.clone()
        };
        assert_eq!(
            message(with_scratch.check()),
            "--upper ram and --scratch are mutually exclusive: an upper in RAM has no scratch \
             image"
        );
        let without_layer = SandboxOptions {
            layers: false,
            ..ram
        };
        assert_eq!(
            message(without_layer.check()),
            format!("sandbox {operation} requires --layer")
        );
    }
    // A provisioned sandbox keeps the upper that its provision recorded.
    for operation in [
        SandboxOperation::Start,
        SandboxOperation::Exec,
        SandboxOperation::Stop,
        SandboxOperation::Deprovision,
    ] {
        for upper in [RootUpper::Scratch, RootUpper::Ram] {
            let options = SandboxOptions {
                upper: Some(upper),
                ..defaults(operation)
            };
            assert_eq!(
                message(options.check()),
                "--upper is only valid for sandbox run or provision"
            );
        }
    }
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
            ..defaults(operation)
        };
        assert_eq!(
            message(options.check()),
            "--mount is only valid for sandbox run or provision"
        );
    }
}

/// Host directories for shares in a temporary directory.
struct Shares {
    /// The temporary directory.
    directory: tempfile::TempDir,
    /// The directory, resolved.
    root: PathBuf,
}

impl Shares {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = resolve(directory.path()).unwrap();
        Self { directory, root }
    }

    fn path(&self, name: &str) -> String {
        let _ = &self.directory;
        let path = paths::normalized(&self.root.join(name));
        path.to_str().unwrap().to_owned()
    }

    fn directory(&self, name: &str) -> String {
        let path = self.path(name);
        fs::create_dir_all(&path).unwrap();
        path
    }

    /// Runs the steps of `nvx-rs sandbox --dry-run` that plan these shares, and returns the
    /// OpenVMM arguments that export them and the kernel token that counts the mappings that the
    /// host sends once the guest is ready.
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
        let mounts = sandbox_mounts(&mounts, &policy_paths, owner)?;
        validate_mounts(&mounts)?;
        let export = plan_mounts(&mounts)?
            .expect("shares have an export plan")
            .description;
        Ok((export.openvmm_arguments(), export.kernel_token()))
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
        values(&arguments, "--mount-child"),
        [format!("0,{share},rw")]
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
    let (arguments, token) = shares
        .run(
            &[format!("/etc/app/settings.json,{settings},ro")],
            &[],
            None,
        )
        .unwrap();
    // The regular file is a child of its own, without its directory, which OpenVMM must find to
    // be a regular file, and the guest maps it from the table that the kernel token counts.
    assert!(!arguments.contains(&"--mount".to_owned()));
    assert_eq!(
        values(&arguments, "--mount-child"),
        [format!("0,{settings},ro,file")]
    );
    assert_eq!(token, "nvx_maps=1");
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
    assert_eq!(
        values(&arguments, "--mount-child"),
        [format!("0,{alpha},rw")]
    );
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
    // The children follow their resolved host paths, and are named by their positions.
    let (report, work) = ("0", "1");
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
            ("--mount-deny".to_owned(), shares.path("2026, final/secret")),
            ("--mount-deny".to_owned(), shares.path("alpha,beta/secret")),
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
    #[cfg(windows)]
    assert!(message(result).contains("Linux host"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let owner = std::fs::metadata(&share).unwrap();
        if owner.uid() == 0 || owner.gid() == 0 {
            // Caller ownership refuses a root-owned share.
            return;
        }
        let (arguments, _) = result.unwrap();
        assert_eq!(values(&arguments, "--mount-owner"), ["caller"]);
        let position = |option: &str| arguments.iter().position(|argument| argument == option);
        assert!(position("--mount-aggregate") < position("--mount-owner"));
    }
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
    let share = |value: &str, requested: MountPolicy| {
        SandboxMount::parse(value, requested, MountOwner::Vmm, MountKind::Directory).unwrap()
    };
    assert_eq!(
        parsed,
        [
            share(
                &mounts[0],
                mount_policy(&["logs"], &["logs/payloads"], &["out"])
            ),
            share(&mounts[1], mount_policy(&["credentials"], &[], &[])),
        ]
    );
    let (arguments, _) = shares.run(&mounts, &policy, None).unwrap();
    // With several shares, OpenVMM attributes absolute paths.
    assert_eq!(
        values(&arguments, "--mount-allow"),
        [shares.path("work/logs/payloads")]
    );
    assert_eq!(
        values(&arguments, "--mount-write"),
        [shares.path("work/out")]
    );

    let workspace = format!("/workspace,{work},rw");
    let toolcache = format!("/opt/hostedtoolcache,{tools}");
    let cache = shares.path("tools/cache");
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
    let (toolcache, workspace) = ("0", "1");
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
    let (arguments, token) = shares.run(&mounts, &policy, None).unwrap();
    // The children follow their resolved host paths, each with its own denied paths, and the
    // kernel token counts their mappings rather than naming their targets.
    let (toolcache, workspace) = ("0", "1");
    assert!(!arguments.contains(&"--mount".to_owned()));
    assert_eq!(
        values(&arguments, "--mount-aggregate"),
        [crate::host::filesystem::GUEST_EXPORT]
    );
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
            shares.path("tools/credentials"),
            shares.path("tools/logs"),
            shares.path("work/secrets"),
        ]
    );
    assert_eq!(token, "nvx_maps=2");

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
    assert_eq!(
        single,
        [SandboxMount::parse(
            &format!("/w,{work},rw"),
            mount_policy(&["secrets"], &[], &[]),
            MountOwner::Vmm,
            MountKind::Directory,
        )
        .unwrap()]
    );
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
        if mount.host_path() == Path::new("b.json") {
            MountKind::File
        } else {
            MountKind::Directory
        }
    };
    let parsed = sandbox_mounts_with(&mounts, &[], Some(MountOwner::Caller), &kind_of).unwrap();
    let share = |target, host_path, access, kind| {
        SandboxMount::new(
            target,
            host_path,
            access,
            MountPolicy::default(),
            MountOwner::Caller,
            kind,
        )
        .unwrap()
    };
    assert_eq!(
        parsed,
        [
            share("/a", "a", MountAccess::ReadOnly, MountKind::Directory),
            share("/b", "b.json", MountAccess::ReadWrite, MountKind::File),
        ]
    );
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
        "/a b", "relative", "/", "/a//b", "/a/./b", "/a\\b", "/a=b", "/a,b", "/a\"b",
    ] {
        assert_eq!(
            message(validate_run_share_targets(&[target, "/b"])),
            format!(
                "invalid --mount target {target:?}: with several shares, or a shared file, each target \
                 must be an absolute non-root path without empty, dot, or parent components, \
                 whitespace, control characters, quotes, '\\', '=', or ','",
            )
        );
    }
    // Unlike a sandbox target, a run target has no length limit.
    validate_run_share_targets(&[&format!("/{}", "x".repeat(5000))]).unwrap();
}

/// A cold boot renders each target into an `nvx_share=` token. Linux would split that token at a
/// byte that it reads as whitespace, and a quote would make it read the rest of the command line,
/// with the virtio-mmio devices that OpenVMM describes after it, as part of the token.
#[test]
fn run_share_targets_remain_one_kernel_token() {
    // U+00A0, U+00E0, and U+0120 end in the byte 0xa0 in UTF-8, which Linux reads as whitespace.
    for separator in [
        '\0', '\t', '\n', '\u{b}', '\u{c}', '\r', ' ', '"', '\u{a0}', '\u{e0}', '\u{120}',
        '\u{3000}', '\u{1c}',
    ] {
        let target = format!("/data{separator}x");
        assert_eq!(
            message(validate_run_share_targets(&[&target, "/logs"])),
            format!(
                "invalid --mount target {target:?}: with several shares, or a shared file, each target \
                 must be an absolute non-root path without empty, dot, or parent components, \
                 whitespace, control characters, quotes, '\\', '=', or ','",
            )
        );
    }
}
