//! Tests of the discovery of artifacts, of the directories that hold them, and of their
//! identities.

use std::collections::HashMap;

use super::*;
use crate::ErrorKind;
use crate::host::store::ArtifactIdentity;

const COMPATIBLE_MANIFEST: &str = r#"{"openvmm":{"microvm_abi_version":2,
        "time_abi_version":2,"restore_packet_version":5,
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

fn no_bundle() -> Result<Option<Artifacts>> {
    Ok(None)
}

fn variables(pairs: &[(&str, &Path)]) -> impl Fn(&str) -> Option<OsString> {
    let map: HashMap<String, OsString> = pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.as_os_str().to_owned()))
        .collect();
    move |name| map.get(name).cloned()
}

#[test]
fn release_layout_names_the_guest_artifacts() {
    let directory = tempfile::tempdir().unwrap();
    release(directory.path());
    let artifacts = Artifacts::from_dir(directory.path()).unwrap();
    let root = std::path::absolute(directory.path()).unwrap();
    assert_eq!(
        artifacts.openvmm,
        root.join("bin").join(openvmm_executable())
    );
    assert_eq!(artifacts.kernel, root.join("guest").join("vmlinux"));
    assert_eq!(
        artifacts.initrd,
        root.join("guest").join("initramfs.cpio.gz")
    );
    // A directory that the caller hands over, such as a `TempDir`, lives until the
    // constructor returns, after it reads the manifest.
    assert_eq!(Artifacts::from_release_dir(directory).unwrap(), artifacts);
}

#[test]
fn repository_layout_is_detected_beside_the_guest_sources() {
    let directory = tempfile::tempdir().unwrap();
    checkout(directory.path());
    let root = std::path::absolute(directory.path()).unwrap();
    let expected = |artifacts: &Artifacts| {
        assert_eq!(artifacts.kernel, root.join("build").join("vmlinux"));
        assert_eq!(
            artifacts.initrd,
            root.join("build").join("initramfs.cpio.gz")
        );
        assert_eq!(
            artifacts.openvmm,
            root.join("openvmm")
                .join("target")
                .join("release")
                .join(openvmm_executable())
        );
    };
    expected(&Artifacts::from_dir(directory.path()).unwrap());
    expected(&Artifacts::from_repo_layout(directory.path()).unwrap());

    // Build outputs do not turn a checkout into a release.
    let build = directory.path().join("build");
    fs::create_dir_all(&build).unwrap();
    fs::write(build.join("vmlinux"), b"kernel").unwrap();
    fs::write(build.join("initramfs.cpio.gz"), b"initrd").unwrap();
    expected(&Artifacts::from_dir(directory.path()).unwrap());
}

#[test]
fn the_directory_variable_accepts_a_repository_checkout() {
    let directory = tempfile::tempdir().unwrap();
    checkout(directory.path());
    let artifacts = discover_with(
        variables(&[(Artifacts::ENV_DIR, directory.path())]),
        None,
        no_bundle,
    )
    .unwrap();
    let build = std::path::absolute(directory.path()).unwrap().join("build");
    assert_eq!(artifacts.kernel, build.join("vmlinux"));
}

#[test]
fn this_repository_uses_the_repository_layout() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    if root.join(MANIFEST_NAME).is_file() {
        let artifacts = Artifacts::from_dir(root).unwrap();
        let kernel = std::path::absolute(root).unwrap().join("build");
        assert_eq!(artifacts.kernel, kernel.join("vmlinux"));
        let directory = ArtifactDir::from_dir(root).unwrap();
        assert_eq!(directory.layout(), Layout::Repository);
        assert_eq!(directory.kernel(), artifacts.kernel);
    }
}

#[test]
fn explicit_variables_win_and_must_be_complete() {
    let directory = tempfile::tempdir().unwrap();
    release(directory.path());
    let (openvmm, kernel, initrd) = (Path::new("o"), Path::new("k"), Path::new("i"));
    let artifacts = discover_with(
        variables(&[
            (Artifacts::ENV_OPENVMM, openvmm),
            (Artifacts::ENV_KERNEL, kernel),
            (Artifacts::ENV_INITRD, initrd),
            (Artifacts::ENV_DIR, directory.path()),
        ]),
        None,
        no_bundle,
    )
    .unwrap();
    assert_eq!(artifacts, Artifacts::new(openvmm, kernel, initrd));

    let error = discover_with(
        variables(&[(Artifacts::ENV_OPENVMM, openvmm)]),
        None,
        no_bundle,
    )
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable);
}

#[test]
fn the_directory_variable_precedes_the_executable_directory() {
    let variable_dir = tempfile::tempdir().unwrap();
    release(variable_dir.path());
    let exe_dir = tempfile::tempdir().unwrap();
    release(&exe_dir.path().join(Artifacts::BESIDE_EXE_DIR));

    let artifacts = discover_with(
        variables(&[(Artifacts::ENV_DIR, variable_dir.path())]),
        Some(exe_dir.path()),
        no_bundle,
    )
    .unwrap();
    assert!(
        artifacts
            .kernel
            .starts_with(std::path::absolute(variable_dir.path()).unwrap())
    );

    let artifacts = discover_with(variables(&[]), Some(exe_dir.path()), no_bundle).unwrap();
    assert!(
        artifacts.kernel.starts_with(
            std::path::absolute(exe_dir.path().join(Artifacts::BESIDE_EXE_DIR)).unwrap()
        )
    );
}

#[test]
fn the_bundle_is_the_last_resort() {
    let exe_dir = tempfile::tempdir().unwrap();
    let bundle = Artifacts::new("bundled-openvmm", "bundled-kernel", "bundled-initrd");
    let expected = bundle.clone();
    let artifacts =
        discover_with(variables(&[]), Some(exe_dir.path()), || Ok(Some(bundle))).unwrap();
    assert_eq!(artifacts, expected);

    let error = discover_with(variables(&[]), Some(exe_dir.path()), no_bundle).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable);
    assert!(error.message().contains(Artifacts::ENV_DIR));
    assert!(
        error
            .message()
            .contains("build the nvx crate with the bundled feature")
    );
}

#[test]
fn incompatible_manifests_are_unavailable() {
    let directory = tempfile::tempdir().unwrap();
    let manifest = directory.path().join(MANIFEST_NAME);
    fs::write(
        &manifest,
        r#"{"openvmm":{"microvm_abi_version":2,"time_abi_version":2,
               "restore_packet_version":5,"control_session_protocol_version":2,
               "control_contract_revision":"nvx-microvm-v2-control-v2"}}"#,
    )
    .unwrap();
    let error = check_manifest(&manifest).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable);
    let error = check_manifest(&directory.path().join("missing.json")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable);
}

#[test]
fn repository_manifest_declares_the_supported_contract() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join(MANIFEST_NAME);
    if manifest.is_file() {
        check_manifest(&manifest).unwrap();
    }
}

#[test]
fn guests_name_their_artifacts_as_nvx_py_does() {
    assert_eq!(
        Guest::ALL.map(Guest::name),
        ["alpine", "ubuntu", "azurelinux"]
    );
    for guest in Guest::ALL {
        assert_eq!(Guest::from_name(guest.name()).unwrap(), guest);
        assert_eq!(guest.to_string(), guest.name());
    }
    let error = Guest::from_name("Ubuntu").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    assert_eq!(
        error.message(),
        "unsupported guest \"Ubuntu\"; choose alpine, ubuntu, azurelinux"
    );
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

    // The release's artifacts are the ones that discovery finds for its directory.
    let discovered = Artifacts::from_dir(directory.path()).unwrap();
    assert_eq!(artifacts.sandbox_artifacts().unwrap(), discovered);
}

#[test]
fn guest_artifacts_are_required_in_the_order_and_words_of_nvx_py() {
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
        artifacts.sandbox_artifacts(),
        "OpenVMM release binary",
        artifacts.openvmm(),
    );
    create(artifacts.openvmm());
    missing(
        artifacts.sandbox_artifacts(),
        "Linux direct kernel",
        artifacts.kernel(),
    );
    create(artifacts.kernel());
    missing(
        artifacts.sandbox_artifacts(),
        "initramfs",
        artifacts.initramfs(Guest::Alpine),
    );
    create(artifacts.initramfs(Guest::Alpine));
    let located = artifacts.sandbox_artifacts().unwrap();
    assert_eq!(
        [&located.openvmm, &located.kernel, &located.initrd],
        [
            &artifacts.openvmm(),
            &artifacts.kernel(),
            &artifacts.initramfs(Guest::Alpine)
        ]
    );
    assert_eq!(located.artifact_dir(), Some(&artifacts));
    assert_eq!(
        located,
        Artifacts::from_repo_layout(directory.path()).unwrap()
    );

    assert_eq!(artifacts.require_openvmm().unwrap(), artifacts.openvmm());
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

/// Returns the identity of artifacts whose SHA-256 digests are `digests`.
fn identity_of(digests: &[String; 3]) -> RuntimeIdentity {
    let [openvmm, kernel, initramfs] = digests
        .each_ref()
        .map(|digest| ArtifactIdentity::sha256(digest).unwrap());
    RuntimeIdentity::new(openvmm, kernel, initramfs)
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

#[test]
fn located_artifacts_have_the_identity_of_their_directory() {
    let directory = tempfile::tempdir().unwrap();
    release(directory.path());
    let digests = ["4", "5", "6"].map(|digit| digit.repeat(64));
    let text: String = [
        (&digests[0], format!("bin/{}", openvmm_executable())),
        (&digests[1], "guest/vmlinux".to_owned()),
        (&digests[2], "guest/initramfs.cpio.gz".to_owned()),
    ]
    .iter()
    .map(|(digest, name)| format!("{digest}  {name}\n"))
    .collect();
    fs::write(directory.path().join(CHECKSUMS_NAME), text).unwrap();
    let located = Artifacts::from_dir(directory.path()).unwrap();
    assert_eq!(
        located.artifact_dir(),
        Some(&ArtifactDir::release(directory.path()).unwrap())
    );
    assert_eq!(located.runtime_identity().unwrap(), identity_of(&digests));

    // Artifacts named by path have no install metadata.
    let named = Artifacts::new(&located.openvmm, &located.kernel, &located.initrd);
    assert_eq!(named.artifact_dir(), None);
    let error = named.runtime_identity().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable);
    assert!(
        error.message().contains("supply their runtime identity"),
        "{error}"
    );
    // Nor does an artifact that replaced one of the directory's.
    let mut replaced = located.clone();
    replaced.kernel = directory.path().join("vmlinux");
    let error = replaced.runtime_identity().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable);
    assert_eq!(
        error.message(),
        format!(
            "the guest kernel {} is not the one of {}, whose install metadata identifies the \
             artifacts",
            replaced.kernel.display(),
            located.artifact_dir().unwrap().root().display()
        )
    );
}

#[cfg(feature = "bundled")]
#[test]
fn the_bundle_is_a_compatible_release() {
    let bundled = Artifacts::bundled().unwrap();
    // A build that was asked to stage a release must have staged it.
    let requested = env::var_os("NVX_BUNDLE_DIR").is_some()
        && env::var_os("NVX_BUNDLE").is_none_or(|value| value != "skip");
    if requested && (cfg!(target_os = "linux") || cfg!(windows)) {
        assert!(bundled.is_some(), "NVX_BUNDLE_DIR was not staged");
    }
    if let Some(artifacts) = bundled {
        for path in [&artifacts.openvmm, &artifacts.kernel, &artifacts.initrd] {
            assert!(path.is_file(), "{} is missing", path.display());
        }
    }
}
