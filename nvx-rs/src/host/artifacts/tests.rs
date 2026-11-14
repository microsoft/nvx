//! Tests of the directories that hold the artifacts, of the guests' artifacts, and of their
//! identities.

use std::fs;

use serde_json::Value;

use super::*;
use crate::ErrorKind;
use crate::host::runtime::{ArtifactIdentity, RuntimeIdentity};

const COMPATIBLE_MANIFEST: &str = r#"{"openvmm":{"microvm_abi_version":1,
        "time_abi_version":1,"restore_packet_version":1,
        "control_session_protocol_version":1,
        "control_contract_revision":"nvx-microvm-v2-control-v2"}}"#;

fn release(root: &Path) {
    for artifact in [
        Path::new("bin").join(openvmm_executable()),
        Path::new("guest").join("vmlinux"),
        Path::new("guest").join("initramfs.cpio.gz"),
    ] {
        let path = root.join(artifact);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"artifact").unwrap();
    }
    fs::write(root.join(MANIFEST_NAME), COMPATIBLE_MANIFEST).unwrap();
}

/// Creates a repository checkout with its tracked guest sources and no build outputs.
fn checkout(root: &Path) {
    let sources = root.join("guest").join("common");
    fs::create_dir_all(&sources).unwrap();
    fs::write(sources.join("init"), b"#!/bin/sh").unwrap();
    fs::write(root.join(MANIFEST_NAME), COMPATIBLE_MANIFEST).unwrap();
}

#[test]
fn repository_layout_is_detected_beside_the_guest_sources() {
    let directory = tempfile::tempdir().unwrap();
    checkout(directory.path());
    let root = std::path::absolute(directory.path()).unwrap();
    let expected = |artifacts: &ArtifactDir| {
        assert_eq!(artifacts.layout(), Layout::Repository);
        assert_eq!(artifacts.kernel(), root.join("build").join("vmlinux"));
        assert_eq!(
            artifacts.initramfs(Guest::Alpine),
            root.join("build").join("initramfs.cpio.gz")
        );
        assert_eq!(
            artifacts.openvmm(),
            root.join("openvmm")
                .join("target")
                .join("release")
                .join(openvmm_executable())
        );
    };
    expected(&ArtifactDir::from_dir(directory.path()).unwrap());

    // Build outputs do not turn a checkout into a release.
    let build = directory.path().join("build");
    fs::create_dir_all(&build).unwrap();
    fs::write(build.join("vmlinux"), b"kernel").unwrap();
    fs::write(build.join("initramfs.cpio.gz"), b"initrd").unwrap();
    expected(&ArtifactDir::from_dir(directory.path()).unwrap());
}

#[test]
fn this_repository_uses_the_repository_layout() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    if root.join(MANIFEST_NAME).is_file() {
        let directory = ArtifactDir::from_dir(root).unwrap();
        assert_eq!(directory.layout(), Layout::Repository);
        let build = std::path::absolute(root).unwrap().join("build");
        assert_eq!(directory.kernel(), build.join("vmlinux"));
    }
}

#[test]
fn guests_name_their_artifacts_as_nvx_py_does() {
    assert_eq!(
        Guest::ALL.map(Guest::name),
        ["alpine", "ubuntu", "azurelinux"]
    );
    for guest in Guest::ALL {
        assert_eq!(guest.to_string(), guest.name());
    }
    assert_eq!(
        Guest::ALL.map(Guest::distribution),
        ["Alpine Linux", "Ubuntu Base", "Azure Linux"]
    );
    assert_eq!(
        Guest::ALL.map(Guest::package_manifest_name),
        [
            "initramfs.cpio.gz.packages.json",
            "initramfs-ubuntu.cpio.gz.packages.json",
            "initramfs-azurelinux.cpio.gz.packages.json",
        ]
    );
    assert_eq!(
        Guest::ALL.map(Guest::distro_layer_name),
        [None, Some("ubuntu-distro.erofs"), None]
    );
    assert_eq!(
        Guest::Ubuntu.distro_manifest_name().as_deref(),
        Some("ubuntu-distro.erofs.manifest.json")
    );
    assert_eq!(Guest::Alpine.distro_manifest_name(), None);
    assert_eq!(Guest::ALL.map(Guest::default_memory_mib), [128, 512, 512]);
}

#[test]
fn a_checkout_holds_the_guest_artifacts_in_build() {
    let directory = tempfile::tempdir().unwrap();
    checkout(directory.path());
    let artifacts = ArtifactDir::from_dir(directory.path()).unwrap();
    let root = std::path::absolute(directory.path()).unwrap();
    let build = root.join("build");
    assert_eq!(artifacts.layout(), Layout::Repository);
    assert_eq!(artifacts.root(), root);
    assert_eq!(
        artifacts.openvmm(),
        root.join("openvmm")
            .join("target")
            .join("release")
            .join(openvmm_executable())
    );
    assert_eq!(artifacts.kernel(), build.join("vmlinux"));
    assert_eq!(
        Guest::ALL.map(|guest| artifacts.initramfs(guest)),
        [
            build.join("initramfs.cpio.gz"),
            build.join("initramfs-ubuntu.cpio.gz"),
            build.join("initramfs-azurelinux.cpio.gz"),
        ]
    );
    assert_eq!(
        artifacts.package_manifest(Guest::Ubuntu),
        build.join("initramfs-ubuntu.cpio.gz.packages.json")
    );
    assert_eq!(
        Guest::ALL.map(|guest| artifacts.distro_layer(guest)),
        [None, Some(build.join("ubuntu-distro.erofs")), None]
    );
    assert_eq!(
        artifacts.distro_manifest(Guest::Ubuntu),
        Some(build.join("ubuntu-distro.erofs.manifest.json"))
    );
    assert_eq!(
        artifacts.artifact("vmlinux.provenance.json"),
        build.join("vmlinux.provenance.json")
    );

    // A relative directory is made absolute, and an explicit layout overrides detection.
    let relative = ArtifactDir::repository(".").unwrap();
    assert_eq!(relative.root(), std::path::absolute(".").unwrap());
    release(directory.path());
    assert_eq!(
        ArtifactDir::repository(directory.path()).unwrap().layout(),
        Layout::Repository
    );
}

#[test]
fn a_release_holds_the_guest_artifacts_in_guest() {
    let directory = tempfile::tempdir().unwrap();
    release(directory.path());
    let artifacts = ArtifactDir::from_dir(directory.path()).unwrap();
    let root = std::path::absolute(directory.path()).unwrap();
    let guest = root.join("guest");
    assert_eq!(artifacts.layout(), Layout::Release);
    assert_eq!(artifacts, ArtifactDir::release(directory.path()).unwrap());
    assert_eq!(
        artifacts.openvmm(),
        root.join("bin").join(openvmm_executable())
    );
    assert_eq!(artifacts.kernel(), guest.join("vmlinux"));
    assert_eq!(
        Guest::ALL.map(|guest| artifacts.initramfs(guest)),
        [
            guest.join("initramfs.cpio.gz"),
            guest.join("initramfs-ubuntu.cpio.gz"),
            guest.join("initramfs-azurelinux.cpio.gz"),
        ]
    );
    assert_eq!(
        artifacts.distro_layer(Guest::Ubuntu),
        Some(guest.join("ubuntu-distro.erofs"))
    );
}

#[test]
fn guest_artifacts_are_required_in_the_words_of_nvx_py() {
    let directory = tempfile::tempdir().unwrap();
    checkout(directory.path());
    let artifacts = ArtifactDir::repository(directory.path()).unwrap();
    fn missing<T: std::fmt::Debug>(result: Result<T>, description: &str, path: PathBuf) {
        let error = result.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unavailable);
        assert_eq!(
            error.message(),
            format!("{description} not found: {}", path.display())
        );
    }
    let create = |path: PathBuf| {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"artifact").unwrap();
    };
    missing(
        artifacts.require_kernel(),
        "Linux direct kernel",
        artifacts.kernel(),
    );
    create(artifacts.kernel());
    assert_eq!(artifacts.require_kernel().unwrap(), artifacts.kernel());
    missing(
        artifacts.require_initramfs(Guest::Ubuntu),
        "Ubuntu Base initramfs",
        artifacts.initramfs(Guest::Ubuntu),
    );
    missing(
        artifacts.require_initramfs(Guest::AzureLinux),
        "Azure Linux initramfs",
        artifacts.initramfs(Guest::AzureLinux),
    );
    create(artifacts.initramfs(Guest::Ubuntu));
    assert_eq!(
        artifacts.require_initramfs(Guest::Ubuntu).unwrap(),
        artifacts.initramfs(Guest::Ubuntu)
    );

    // A directory is not a regular file.
    fs::create_dir_all(artifacts.initramfs(Guest::AzureLinux)).unwrap();
    missing(
        artifacts.require_initramfs(Guest::AzureLinux),
        "Azure Linux initramfs",
        artifacts.initramfs(Guest::AzureLinux),
    );
}

#[test]
fn artifact_paths_serialize_every_guest_in_order() {
    let directory = tempfile::tempdir().unwrap();
    release(directory.path());
    let artifacts = ArtifactDir::from_dir(directory.path()).unwrap();
    let text = |path: PathBuf| Value::from(path.to_str().unwrap());
    let value = serde_json::to_value(artifacts.paths()).unwrap();
    assert_eq!(value["layout"], "release");
    assert_eq!(value["root"], text(artifacts.root().to_path_buf()));
    assert_eq!(value["openvmm"], text(artifacts.openvmm()));
    assert_eq!(value["kernel"], text(artifacts.kernel()));
    let guests = value["guests"].as_array().unwrap();
    assert_eq!(guests.len(), Guest::ALL.len());
    for (paths, guest) in guests.iter().zip(Guest::ALL) {
        assert_eq!(paths["name"], guest.name());
        assert_eq!(paths["distribution"], guest.distribution());
        assert_eq!(paths["initramfs"], text(artifacts.initramfs(guest)));
        assert_eq!(
            paths["package_manifest"],
            text(artifacts.package_manifest(guest))
        );
        assert_eq!(
            paths["distro_layer"],
            artifacts.distro_layer(guest).map_or(Value::Null, text)
        );
        assert_eq!(
            paths["distro_manifest"],
            artifacts.distro_manifest(guest).map_or(Value::Null, text)
        );
    }
    let repository = ArtifactDir::repository(directory.path()).unwrap();
    assert_eq!(
        serde_json::to_value(repository.paths()).unwrap()["layout"],
        "repository"
    );
}

/// Returns the identity of artifacts whose SHA-256 digests are `digests`, which the CLI drives
/// under its compatibility with its bundled guests.
fn identity_of(digests: &[String; 3]) -> RuntimeIdentity {
    let [openvmm, kernel, initramfs] = digests
        .each_ref()
        .map(|digest| ArtifactIdentity::sha256(digest).unwrap());
    RuntimeIdentity::new(
        openvmm,
        kernel,
        initramfs,
        compatibility::compatibility().unwrap(),
    )
}

/// Writes the build provenance of the artifacts of the checkout at `root`, as `nvx.py`
/// records it, with `digests` for the OpenVMM executable, the kernel, and the initramfs.
fn record_provenance(root: &Path, digests: &[String; 3]) {
    let build = root.join("build");
    fs::create_dir_all(&build).unwrap();
    for ((name, field), digest) in [
        (OPENVMM_PROVENANCE_NAME, "executable_sha256"),
        (KERNEL_PROVENANCE_NAME, "kernel_sha256"),
        (INITRAMFS_PROVENANCE_NAME, "initramfs_sha256"),
    ]
    .into_iter()
    .zip(digests)
    {
        let text = format!("{{\n  \"format\": 1,\n  \"{field}\": \"{digest}\"\n}}\n");
        fs::write(build.join(name), text).unwrap();
    }
}

#[test]
fn a_checkout_identifies_its_runtime_by_the_build_provenance_of_its_artifacts() {
    let directory = tempfile::tempdir().unwrap();
    checkout(directory.path());
    let artifacts = ArtifactDir::repository(directory.path()).unwrap();
    let digests = ["a", "b", "c"].map(|digit| digit.repeat(64));
    // Only the metadata counts: the artifacts themselves are neither read nor hashed.
    record_provenance(directory.path(), &digests);
    assert_eq!(artifacts.runtime_identity().unwrap(), identity_of(&digests));

    let kernel = artifacts.artifact(KERNEL_PROVENANCE_NAME);
    let fails = |content: Option<&str>, problem: &str| {
        match content {
            Some(content) => fs::write(&kernel, content).unwrap(),
            None => fs::remove_file(&kernel).unwrap(),
        }
        let error = artifacts.runtime_identity().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unavailable);
        assert_eq!(
            error.message(),
            format!("the kernel build provenance {} {problem}", kernel.display())
        );
    };
    fails(
        None,
        "is missing, so the runtime cannot be identified: build the artifact with nvx.py, or \
         install a release with nvx-rs download",
    );
    fails(Some("{"), "is not valid JSON");
    let digest = &digests[1];
    fails(
        Some(&format!(r#"{{"format": 2, "kernel_sha256": "{digest}"}}"#)),
        "does not have format 1",
    );
    fails(
        Some(r#"{"format": 1, "initramfs_sha256": "0"}"#),
        "has no SHA-256 digest in kernel_sha256",
    );
    let upper = digest.to_ascii_uppercase();
    fails(
        Some(&format!(r#"{{"format": 1, "kernel_sha256": "{upper}"}}"#)),
        "has no SHA-256 digest in kernel_sha256",
    );
}

#[test]
fn a_release_identifies_its_runtime_by_its_checksum_list() {
    let directory = tempfile::tempdir().unwrap();
    release(directory.path());
    let artifacts = ArtifactDir::release(directory.path()).unwrap();
    let digests = ["1", "2", "3"].map(|digit| digit.repeat(64));
    let other = "f".repeat(64);
    let openvmm = format!("bin/{}", openvmm_executable());
    let list = artifacts.root().join(CHECKSUMS_NAME);
    let write = |lines: &[(&str, &str)]| {
        let text: String = lines
            .iter()
            .map(|(digest, name)| format!("{digest}  {name}\n"))
            .collect();
        fs::write(&list, text).unwrap();
    };
    let complete = [
        (other.as_str(), "SOURCE-MANIFEST.json"),
        (digests[0].as_str(), openvmm.as_str()),
        (other.as_str(), "guest/initramfs-ubuntu.cpio.gz"),
        (digests[2].as_str(), "guest/initramfs.cpio.gz"),
        (digests[1].as_str(), "guest/vmlinux"),
    ];
    write(&complete);
    let identity = artifacts.runtime_identity().unwrap();
    assert_eq!(identity, identity_of(&digests));

    // A checkout that installed the release has the release's identity.
    let installed = tempfile::tempdir().unwrap();
    checkout(installed.path());
    record_provenance(installed.path(), &digests);
    let checkout = ArtifactDir::repository(installed.path()).unwrap();
    assert_eq!(checkout.runtime_identity().unwrap(), identity);

    let fails = |problem: String| {
        let error = artifacts.runtime_identity().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unavailable);
        assert_eq!(
            error.message(),
            format!("the release checksum list {} {problem}", list.display())
        );
    };
    write(&complete[..4]);
    fails("does not list guest/vmlinux".to_owned());
    let upper = other.to_ascii_uppercase();
    write(&[(upper.as_str(), "guest/vmlinux")]);
    fails(format!(
        "has a malformed line: {:?}",
        format!("{upper}  guest/vmlinux")
    ));
    write(&[(digests[1].as_str(), "")]);
    fails(format!(
        "has a malformed line: {:?}",
        format!("{}  ", digests[1])
    ));
    write(&[
        (digests[1].as_str(), "guest/vmlinux"),
        (other.as_str(), "guest/vmlinux"),
    ]);
    fails("lists guest/vmlinux twice".to_owned());
    fs::remove_file(&list).unwrap();
    fails("is missing, so the runtime cannot be identified: the release is incomplete".to_owned());
}

#[cfg(unix)]
#[test]
fn metadata_that_is_not_a_regular_file_fails_at_once() {
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use nix::sys::stat::Mode;

    let installed = tempfile::tempdir().unwrap();
    checkout(installed.path());
    record_provenance(
        installed.path(),
        &["a", "b", "c"].map(|digit| digit.repeat(64)),
    );
    let repository = ArtifactDir::repository(installed.path()).unwrap();
    let extracted = tempfile::tempdir().unwrap();
    release(extracted.path());
    let released = ArtifactDir::release(extracted.path()).unwrap();
    for (artifacts, path, description) in [
        (
            &repository,
            repository.artifact(KERNEL_PROVENANCE_NAME),
            "kernel build provenance",
        ),
        (
            &released,
            released.root().join(CHECKSUMS_NAME),
            "release checksum list",
        ),
    ] {
        let _ = fs::remove_file(&path);
        nix::unistd::mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        // An open of a FIFO for reading that waits would wait for a writer, and a read of one
        // would wait until its writer closes it, so the identity is read on a thread that the
        // test can leave behind.
        let refused = || {
            let (sender, receiver) = mpsc::channel();
            let artifacts = artifacts.clone();
            thread::spawn(move || sender.send(artifacts.runtime_identity()).unwrap());
            let error = receiver
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|_| panic!("the read of the {description} waits on the FIFO"))
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unavailable);
            assert_eq!(
                error.message(),
                format!("the {description} {} is not a regular file", path.display())
            );
        };
        refused();
        // A FIFO that has a writer opens at once, and is refused all the same, before a read
        // waits for the writer to close it.
        let reader = fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits())
            .open(&path)
            .unwrap();
        let writer = fs::OpenOptions::new()
            .write(true)
            .custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits())
            .open(&path)
            .unwrap();
        drop(reader);
        refused();
        drop(writer);
    }
}
