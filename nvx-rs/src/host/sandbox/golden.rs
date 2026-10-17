//! Checks the launch profile and the option checks against `tests/fixtures/sandbox.json` and
//! `tests/fixtures/sandbox_options.json`, which the retired `scripts/nvx_rs_fixtures.py`
//! recorded from the Python launch profile and option checks of `nvx.py sandbox`, which nvx-rs
//! replaced.
//!
//! The fixtures hold the paths of both `pathlib` flavors, with a fixed current directory, so
//! every case runs on every host. The cases that read files run on files of a temporary
//! directory.

use std::io;

use serde_json::{Value, json};

use super::mount::{
    aggregate_child_name, mounts_openvmm_arguments_with, parse_mount_value, validate_mount_target,
};
use super::options::{
    PolicyPathOption, SandboxOperation, SandboxOptions, sandbox_mounts_with,
    validate_exec_environment, validate_run_share_targets, validate_sandbox_systemd_policy,
};
use super::*;
use crate::host::python::golden::{Mismatches, cases, fixture, flavor, hex, text, texts};

/// Returns the message of a result's error, or `None` if it succeeded.
fn error<T>(result: &Result<T>) -> Option<String> {
    result.as_ref().err().map(ToString::to_string)
}

/// Returns `Path.cwd()` of the fixture for paths of `flavor`.
fn cwd(fixture: &Value, flavor: Flavor) -> impl Fn() -> io::Result<PurePath> {
    let name = match flavor {
        Flavor::Posix => "posix",
        Flavor::Windows => "windows",
    };
    let directory = PurePath::new(flavor, text(&fixture["cwds"], name));
    move || Ok(directory.clone())
}

/// Returns the fields of a share as the generator records them.
fn mount_fields(mount: &SandboxMount) -> Value {
    let policy = mount.policy();
    json!({
        "guest_target": mount.guest_target(),
        "host_path": mount.host_path().to_str().expect("a Unicode path"),
        "access": mount.access().as_str(),
        "denied": policy.denied,
        "allowed": policy.allowed,
        "writable": policy.writable,
        "owner": mount.owner().as_str(),
        "kind": mount.kind().as_str(),
    })
}

/// Parses the share of a generator's specification.
fn parse_mount(flavor: Flavor, spec: &Value) -> Result<SandboxMount> {
    let policy = MountPolicy {
        denied: texts(spec, "denied"),
        allowed: texts(spec, "allowed"),
        writable: texts(spec, "writable"),
    };
    SandboxMount::parse_in(
        flavor,
        text(spec, "value"),
        policy,
        MountOwner::from_name(text(spec, "owner"))?,
        MountKind::from_name(text(spec, "kind"))?,
    )
}

/// Returns the expected outcome of a case: its `key` field, or its `error` field.
fn outcome(case: &Value, key: &str) -> std::result::Result<Value, String> {
    match case.get("error") {
        Some(Value::String(message)) => Err(message.clone()),
        _ => Ok(case[key].clone()),
    }
}

#[test]
fn identities_and_layers_parse_as_python_parses_them() {
    let fixture = fixture("sandbox.json");
    let mut mismatches = Mismatches::new("identities");
    for case in cases(&fixture, "identities") {
        let actual = parse_workload_identity(text(case, "value"))
            .map(|(uid, gid)| json!([uid, gid]))
            .map_err(|error| error.to_string());
        mismatches.check(case, outcome(case, "identity"), actual);
    }
    mismatches.finish();

    let mut mismatches = Mismatches::new("layers");
    for case in cases(&fixture, "layers") {
        let expected =
            outcome(case, "role").map(|_| json!([case["role"], case["path"], case["uuid"]]));
        let actual = SandboxLayer::parse_in(flavor(case), text(case, "value"))
            .map(|layer| {
                json!([
                    layer.role().as_str(),
                    layer.pure_path().as_str(),
                    layer.uuid()
                ])
            })
            .map_err(|error| error.to_string());
        mismatches.check(case, expected, actual);
    }
    mismatches.finish();
}

#[test]
fn mount_values_targets_and_child_names_match_python() {
    let fixture = fixture("sandbox.json");
    let mut mismatches = Mismatches::new("mount_values");
    for case in cases(&fixture, "mount_values") {
        let expected = outcome(case, "target")
            .map(|_| json!([case["target"], case["host_path"], case["access"]]));
        let actual = parse_mount_value(text(case, "value"))
            .map(|(target, host_path, access)| json!([target, host_path, access.as_str()]))
            .map_err(|error| error.to_string());
        mismatches.check(case, expected, actual);
    }
    mismatches.finish();

    let mut mismatches = Mismatches::new("mount_targets");
    for case in cases(&fixture, "mount_targets") {
        mismatches.check(
            case,
            case["error"].as_str().map(str::to_owned),
            error(&validate_mount_target(text(case, "target"))),
        );
    }
    mismatches.finish();

    let mut mismatches = Mismatches::new("child_names");
    for case in cases(&fixture, "child_names") {
        let index = usize::try_from(case["index"].as_u64().unwrap()).unwrap();
        mismatches.check(
            case,
            text(case, "name"),
            aggregate_child_name(index, text(case, "target")).as_str(),
        );
    }
    mismatches.finish();
}

#[test]
fn mounts_lower_to_the_arguments_that_python_gives() {
    let fixture = fixture("sandbox.json");
    let mut mismatches = Mismatches::new("mounts");
    for case in cases(&fixture, "mounts") {
        let flavor = flavor(case);
        let cwd = cwd(&fixture, flavor);
        let expected = outcome(case, "mount").map(|_| {
            json!([
                case["mount"],
                case["arguments"],
                case["fragment"],
                case["absolute"],
                case["absolute_policy"],
            ])
        });
        let actual = parse_mount(flavor, case).and_then(|mount| {
            let policy: Vec<Value> = mount
                .absolute_policy_paths_with(&cwd)?
                .into_iter()
                .map(|(kind, paths)| json!([kind.as_str(), paths]))
                .collect();
            Ok(json!([
                mount_fields(&mount),
                mounts_openvmm_arguments_with(std::slice::from_ref(&mount), &cwd)?,
                mount.command_line_fragment(),
                mount_fields(&mount.absolute_with(&cwd)?),
                policy,
            ]))
        });
        mismatches.check(case, expected, actual.map_err(|error| error.to_string()));
    }
    mismatches.finish();
}

#[test]
fn launches_give_the_arguments_and_command_lines_that_python_gives() {
    let fixture = fixture("sandbox.json");
    let mut mismatches = Mismatches::new("launches");
    for case in cases(&fixture, "launches") {
        let flavor = flavor(case);
        let layers = texts(case, "layers")
            .iter()
            .map(|value| SandboxLayer::parse_in(flavor, value))
            .collect::<Result<Vec<_>>>()
            .expect("the generator records launches whose layers parse");
        let mounts = case["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|spec| parse_mount(flavor, spec))
            .collect::<Result<Vec<_>>>()
            .expect("the generator records launches whose shares parse");
        let limit = |key: &str| case[key].as_str().map(|value| Int::parse(value).unwrap());
        let identity = &case["identity"];
        let id = |index: usize| u32::try_from(identity[index].as_u64().unwrap()).unwrap();
        let settings = SandboxSettings {
            entrypoint: text(case, "entrypoint").to_owned(),
            args: texts(case, "args"),
            hostname: text(case, "hostname").to_owned(),
            workload_identity: (id(0), id(1)),
            memory_max: limit("memory_max"),
            pids_max: limit("pids_max"),
            mounts,
        };
        let expected = match (case.get("error"), case.get("kernel_error")) {
            (Some(message), _) => Err(message.as_str().unwrap().to_owned()),
            (None, kernel_error) => Ok(json!([
                case["arguments"],
                case.get("kernel_command_line").unwrap_or(&Value::Null),
                kernel_error.unwrap_or(&Value::Null),
            ])),
        };
        let actual = SandboxLaunch::new_in(flavor, layers, text(case, "scratch"), settings)
            .and_then(|launch| {
                let arguments = launch.openvmm_arguments_with(&cwd(&fixture, flavor))?;
                let command_line = launch.kernel_command_line(text(case, "command_line"));
                Ok(json!([
                    arguments,
                    command_line.as_ref().ok(),
                    error(&command_line),
                ]))
            })
            .map_err(|error| error.to_string());
        mismatches.check(case, expected, actual);
    }
    mismatches.finish();
}

#[test]
fn sandbox_option_helpers_match_nvx_py() {
    let fixture = fixture("sandbox_options.json");
    let mut mismatches = Mismatches::new("run_share_targets");
    for case in cases(&fixture, "run_share_targets") {
        let targets = texts(case, "targets");
        let targets: Vec<&str> = targets.iter().map(String::as_str).collect();
        mismatches.check(
            case,
            case["error"].as_str().map(str::to_owned),
            error(&validate_run_share_targets(&targets)),
        );
    }
    mismatches.finish();

    let mut mismatches = Mismatches::new("sandbox_mounts");
    for case in cases(&fixture, "sandbox_mounts") {
        let files = texts(case, "files");
        let policy_paths: Vec<PolicyPathOption> = case["policy"]
            .as_array()
            .unwrap()
            .iter()
            .map(|option| PolicyPathOption {
                kind: match option[0].as_str().unwrap() {
                    "denied" => PolicyKind::Denied,
                    "allowed" => PolicyKind::Allowed,
                    _ => PolicyKind::Writable,
                },
                preceding_mount: option[1]
                    .as_u64()
                    .map(|index| usize::try_from(index).unwrap()),
                path: option[2].as_str().unwrap().to_owned(),
            })
            .collect();
        let owner = case["owner"]
            .as_str()
            .map(|owner| MountOwner::from_name(owner).unwrap());
        let kind_of = |mount: &SandboxMount| {
            let host_path = mount.host_path().to_str().unwrap();
            if files.iter().any(|file| file == host_path) {
                MountKind::File
            } else {
                MountKind::Directory
            }
        };
        let actual = sandbox_mounts_with(
            flavor(case),
            &texts(case, "mounts"),
            &policy_paths,
            owner,
            &kind_of,
        )
        .map(|mounts| Value::Array(mounts.iter().map(mount_fields).collect()))
        .map_err(|error| error.to_string());
        mismatches.check(case, outcome(case, "result"), actual);
    }
    mismatches.finish();

    let mut mismatches = Mismatches::new("environments");
    for case in cases(&fixture, "environments") {
        mismatches.check(
            case,
            case["error"].as_str().map(str::to_owned),
            error(&validate_exec_environment(&texts(case, "entries"))),
        );
    }
    mismatches.finish();
}

/// Returns the bytes of a document of the generator: its text, its bytes in hex, or a number
/// of spaces.
fn document(case: &Value) -> Vec<u8> {
    if let Some(text) = case["text"].as_str() {
        text.as_bytes().to_vec()
    } else if let Some(bytes) = case["bytes"].as_str() {
        hex(bytes)
    } else {
        let spaces = case["spaces"].as_u64().expect("a document");
        vec![b' '; usize::try_from(spaces).unwrap()]
    }
}

#[test]
fn environment_files_and_distro_manifests_read_as_python_reads_them() {
    let fixture = fixture("sandbox_options.json");
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("environment.json");
    let path = path.to_str().unwrap();
    let shown = PurePath::native(path).as_str().to_owned();
    let mut mismatches = Mismatches::new("environment_files");
    for case in cases(&fixture, "environment_files") {
        std::fs::write(path, document(case)).unwrap();
        let mut options = SandboxOptions::new(SandboxOperation::Exec);
        options.environment_file = Some(path.to_owned());
        let expected = match case.get("environment") {
            Some(environment) => Ok(environment.clone()),
            None => Err(text(case, "error").to_owned()),
        };
        let actual = options
            .check()
            .map(|environment| json!(environment))
            .map_err(|error| error.to_string().replace(&shown, "{path}"));
        mismatches.check(case, expected, actual);
    }
    mismatches.finish();

    let layer = directory.path().join("distro.erofs");
    std::fs::write(&layer, text(&fixture, "distro_layer")).unwrap();
    let manifest = directory.path().join("distro.erofs.manifest.json");
    let scratch = directory.path().join("scratch.ext4");
    let launch = SandboxLaunch::new(
        vec![SandboxLayer::new(
            LayerRole::Distro,
            layer.to_str().unwrap(),
            "11111111-1111-1111-1111-111111111111",
        )],
        scratch.to_str().unwrap(),
        SandboxSettings {
            entrypoint: "/sbin/init".to_owned(),
            ..SandboxSettings::default()
        },
    )
    .unwrap();
    let mut mismatches = Mismatches::new("distro_manifests");
    for case in cases(&fixture, "distro_manifests") {
        std::fs::write(&manifest, document(&case["manifest"])).unwrap();
        mismatches.check(
            case,
            case["error"].as_str().map(str::to_owned),
            error(&validate_sandbox_systemd_policy(&launch)),
        );
    }
    mismatches.finish();
}
