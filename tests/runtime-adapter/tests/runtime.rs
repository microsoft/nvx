//! The runtime of the adapter's VMs takes NVX's artifacts from a repository checkout or an
//! extracted release, and their runtime identity from NVX's, through the install metadata beside
//! them, with the guest agent's build in the identity of the initramfs.
#![cfg(feature = "host")]

use std::fs;
use std::path::{Path, PathBuf};

use nvx::ErrorKind;
use nvx::host::artifacts::ArtifactDir;
use nvx::host::store::{ArtifactIdentity, IdentityPart, RuntimeIdentity};
use runtime_adapter::GUEST_BUILD;
use runtime_adapter::runtime::Runtime;

/// The digests that the install metadata of the test's artifacts records.
const OPENVMM: &str = "0a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f9";
const KERNEL: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const INITRAMFS: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

/// The build provenance of a checkout's artifacts: each record's name, field, and digest.
const PROVENANCE: [(&str, &str, &str); 3] = [
    ("openvmm.provenance.json", "executable_sha256", OPENVMM),
    ("vmlinux.provenance.json", "kernel_sha256", KERNEL),
    ("initramfs.provenance.json", "initramfs_sha256", INITRAMFS),
];

/// Creates an empty directory for the test `name`.
fn directory(name: &str) -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("runtime")
        .join(name);
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    directory
}

/// Lays out NVX's artifacts in `root`, with NVX's manifest and the install metadata that names
/// them: in the release layout, with its `SHA256SUMS`, if `release`, and otherwise in a
/// repository checkout's, with the build provenance in `build/`. Returns the path of the metadata
/// that names the kernel.
fn lay_out(root: &Path, release: bool) -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../SOURCE-MANIFEST.json");
    fs::copy(manifest, root.join("SOURCE-MANIFEST.json")).unwrap();
    if release {
        // A release is told apart by its built artifacts.
        fs::create_dir_all(root.join("guest")).unwrap();
        fs::write(root.join("guest/vmlinux"), b"kernel").unwrap();
        let openvmm = if cfg!(windows) {
            "bin/openvmm.exe"
        } else {
            "bin/openvmm"
        };
        let sums = format!(
            "{OPENVMM}  {openvmm}\n{KERNEL}  guest/vmlinux\n{INITRAMFS}  guest/initramfs.cpio.gz\n"
        );
        let path = root.join("SHA256SUMS");
        fs::write(&path, sums).unwrap();
        path
    } else {
        let build = root.join("build");
        fs::create_dir_all(&build).unwrap();
        for (name, field, digest) in PROVENANCE {
            let record = format!("{{\"format\": 1, \"{field}\": \"{digest}\", \"other\": true}}");
            fs::write(build.join(name), record).unwrap();
        }
        build.join("vmlinux.provenance.json")
    }
}

/// Returns the identity of the runtime of the test's artifacts with the guest agent's build
/// `guest_build`.
fn identity(guest_build: &str) -> RuntimeIdentity {
    let sha256 = |digest| ArtifactIdentity::sha256(digest).unwrap();
    let initramfs = format!("sha256:{INITRAMFS}+runtime-adapter-guest/{guest_build}");
    RuntimeIdentity::new(
        sha256(OPENVMM),
        sha256(KERNEL),
        ArtifactIdentity::new(initramfs).unwrap(),
    )
}

#[test]
fn the_runtime_identity_is_nvx_s_with_the_guest_agent_s_build_in_either_layout() {
    for (name, release, guest) in [("repository", false, "build"), ("release", true, "guest")] {
        let root = directory(name);
        lay_out(&root, release);
        let runtime = Runtime::from_dir(&root, GUEST_BUILD).unwrap();
        assert_eq!(runtime.identity, identity(GUEST_BUILD), "{name}");
        // Only the initramfs, which holds the guest agent, differs from NVX's identity.
        let nvx = ArtifactDir::from_dir(&root)
            .unwrap()
            .runtime_identity()
            .unwrap();
        assert_eq!(
            runtime.identity.differences(&nvx),
            [IdentityPart::Initramfs],
            "{name}"
        );
        // The VMs boot a copy of NVX's Alpine initramfs.
        let root = fs::canonicalize(&root).unwrap();
        let initrd = fs::canonicalize(runtime.artifacts.initrd.parent().unwrap()).unwrap();
        assert_eq!(initrd, root.join(guest), "{name}");
        assert_eq!(
            runtime.artifacts.initrd.file_name().unwrap(),
            "initramfs.cpio.gz"
        );
    }
}

#[test]
fn another_build_of_the_guest_agent_is_another_runtime() {
    let root = directory("another-build");
    lay_out(&root, false);
    let ours = Runtime::from_dir(&root, GUEST_BUILD).unwrap().identity;
    let theirs = Runtime::from_dir(&root, "another-build").unwrap().identity;
    assert_eq!(theirs, identity("another-build"));
    assert_eq!(ours.differences(&theirs), [IdentityPart::Initramfs]);

    // A build that an artifact identity cannot hold fails.
    for build in ["with space", &"x".repeat(64)] {
        let error = Runtime::from_dir(&root, build).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{build:?}: {error}");
    }
}

#[test]
fn missing_install_metadata_fails_as_nvx_reports_it() {
    for (name, release) in [("missing-provenance", false), ("missing-checksums", true)] {
        let root = directory(name);
        let metadata = lay_out(&root, release);
        fs::remove_file(&metadata).unwrap();
        let error = Runtime::from_dir(&root, GUEST_BUILD).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unavailable, "{name}: {error}");
        let missing = format!("{} is missing", metadata.display());
        assert!(error.message().contains(&missing), "{name}: {error}");
    }
}
