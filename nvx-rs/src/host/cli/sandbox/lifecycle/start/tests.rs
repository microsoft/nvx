//! Tests of the configuration that a run launches, and of the removal of its VM directory.

use std::fs;

use super::*;
use crate::host::paths::resolve;
use crate::host::sandbox::layer::{LayerRole, SandboxLayer};
use crate::host::sandbox::mount::{plan_mounts, validate_mounts};
use crate::host::sandbox::options::{sandbox_mounts, validate_sandbox_systemd_policy};
use crate::host::sandbox::{SandboxSettings, SandboxUpper};

/// The starts of restored sandboxes, with a fake OpenVMM and guest in place of OpenVMM's restore,
/// which the tests fake on Linux and Windows.
#[cfg(any(target_os = "linux", windows))]
mod restore;

#[test]
fn run_checks_again_that_its_shares_do_not_overlap() {
    let directory = tempfile::tempdir().unwrap();
    let root = resolve(directory.path()).unwrap();
    let path = |name: &str| root.join(name).to_str().unwrap().to_owned();
    for (name, content) in [
        ("distro.erofs", "distro"),
        ("scratch.ext4", "scratch"),
        ("settings.json", "{}"),
        ("output.log", ""),
    ] {
        fs::write(path(name), content).unwrap();
    }
    let mounts = sandbox_mounts(
        &[
            format!("/etc/settings.json,{}", path("settings.json")),
            format!("/var/log/output.log,{},rw", path("output.log")),
        ],
        &[],
        None,
    )
    .unwrap();
    validate_mounts(&mounts).unwrap();
    let launch = SandboxLaunch::new(
        vec![SandboxLayer::new(
            LayerRole::Distro,
            &path("distro.erofs"),
            "11111111-1111-1111-1111-111111111111",
        )],
        SandboxUpper::scratch(&path("scratch.ext4")),
        SandboxSettings::default(),
    )
    .unwrap()
    .with_export(plan_mounts(&mounts).unwrap())
    .unwrap();
    let binding = validate_sandbox_systemd_policy(&launch).unwrap();
    let distro = RecordedDistro::new(&launch, binding).unwrap();
    let config = ProvisionConfig {
        hypervisor: "kvm",
        memory_mib: 256,
        cmdline: "",
        ram_upper_percent: None,
        network: None,
    };
    record(&launch, &distro, &config).unwrap();

    // A hard link that replaced the read-only file with the writable one while the run verified
    // its distro layer would export one file under both access modes; the export plan's own
    // checks pass, since a file share's root is not pinned.
    fs::remove_file(path("settings.json")).unwrap();
    fs::hard_link(path("output.log"), path("settings.json")).unwrap();
    launch.export().unwrap().verify().unwrap();
    let Err(error) = record(&launch, &distro, &config) else {
        panic!("the run launched shares that came to overlap");
    };
    assert_eq!(
        error.code(),
        Some(crate::ErrorCode::Unsupported),
        "{error:?}"
    );
    let message = error.message().unwrap_or_default();
    assert!(
        message.contains("name the same directory or file"),
        "{message}"
    );
}

/// A run removes its VM directory with the staging file of an outcome report that OpenVMM did
/// not finish writing, as one that the run forced down can leave, and keeps a directory that
/// holds a file of another name.
#[test]
fn a_run_removes_its_vm_directory_with_a_report_s_staging_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("vm");
    for (staging, removed) in [(".tmpA1b2C3", true), ("other", false)] {
        vm::create_private_dir(&path).unwrap();
        for name in [StateFile::Log.name(), StateFile::Outcome.name(), staging] {
            fs::write(path.join(name), "").unwrap();
        }
        let result = remove_vm_directory(&VmDir::open(&path).unwrap());
        assert_eq!(result.is_ok(), removed, "{staging}: {result:?}");
        assert_eq!(path.exists(), !removed, "{staging}");
        if !removed {
            assert!(path.join(staging).exists(), "{staging}");
            fs::remove_dir_all(&path).unwrap();
        }
    }
}
