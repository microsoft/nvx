use std::fs;

use super::*;
use crate::host::artifacts::{ArtifactDir, Guest, openvmm_executable};

#[test]
#[cfg(all(windows, feature = "cli"))]
fn arm_native_vmm_admission_rejects_foreign_or_truncated_pe_images() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("openvmm.exe");
    let mut bytes = [0; 70];
    bytes[..2].copy_from_slice(b"MZ");
    bytes[60..64].copy_from_slice(&64_u32.to_le_bytes());
    bytes[64..68].copy_from_slice(b"PE\0\0");
    bytes[68..70].copy_from_slice(&0xaa64_u16.to_le_bytes());
    fs::write(&path, bytes).unwrap();
    assert!(crate::host::artifacts::check_arm64_pe(&path).is_ok());
    for machine in [0x8664_u16, 0x14c] {
        bytes[68..70].copy_from_slice(&machine.to_le_bytes());
        fs::write(&path, bytes).unwrap();
        assert!(crate::host::artifacts::check_arm64_pe(&path).is_err());
    }
    bytes[60..64].copy_from_slice(&u32::MAX.to_le_bytes());
    fs::write(&path, bytes).unwrap();
    assert!(crate::host::artifacts::check_arm64_pe(&path).is_err());
    fs::write(&path, b"MZ").unwrap();
    assert!(crate::host::artifacts::check_arm64_pe(&path).is_err());
}

/// A bounded header fixture, not a bootable or qualified kernel.
fn image() -> [u8; 64] {
    let mut image = [0; 64];
    image[16..24].copy_from_slice(&4096_u64.to_le_bytes());
    image[24..32].copy_from_slice(&10_u64.to_le_bytes());
    image[56..60].copy_from_slice(b"ARM\x64");
    image
}

#[test]
fn explicit_arm_paths_never_change_legacy_default_or_openvmm_location() {
    let root = tempfile::tempdir().unwrap();
    let legacy = ArtifactDir::repository(root.path()).unwrap();
    let arm = legacy
        .clone()
        .with_architecture(Architecture::Aarch64)
        .unwrap();
    assert_eq!(legacy.architecture(), Architecture::X86_64);
    assert_eq!(legacy.kernel(), root.path().join("build").join("vmlinux"));
    assert_eq!(
        arm.kernel(),
        root.path().join("build").join("aarch64").join("Image")
    );
    assert_eq!(arm.openvmm(), legacy.openvmm());
    assert_eq!(arm.openvmm().file_name().unwrap(), openvmm_executable());
    assert_eq!(arm.paths().guests.len(), 2);
    assert_eq!(
        arm.distro_layer(Guest::Ubuntu).unwrap(),
        root.path().join("build/aarch64/ubuntu-distro.erofs")
    );
    assert!(arm.sandbox_artifacts().is_err());
}

#[test]
fn arm_managed_artifacts_require_matching_native_kernel_and_manifest() {
    let root = tempfile::tempdir().unwrap();
    let arm = ArtifactDir::repository(root.path())
        .unwrap()
        .with_architecture(Architecture::Aarch64)
        .unwrap();
    fs::create_dir_all(arm.openvmm().parent().unwrap()).unwrap();
    let mut executable = [0; 70];
    executable[..2].copy_from_slice(b"MZ");
    executable[60..64].copy_from_slice(&64_u32.to_le_bytes());
    executable[64..68].copy_from_slice(b"PE\0\0");
    executable[68..70].copy_from_slice(&0xaa64_u16.to_le_bytes());
    fs::write(arm.openvmm(), executable).unwrap();
    fs::create_dir_all(arm.kernel().parent().unwrap()).unwrap();
    fs::write(arm.kernel(), image()).unwrap();
    fs::write(
        arm.initramfs(Guest::Alpine),
        b"unit fixture, not a bootable CPIO",
    )
    .unwrap();
    assert!(arm.sandbox_artifacts().is_err());
    fs::write(
        arm.package_manifest(Guest::Alpine),
        r#"{"architecture":"x86_64"}"#,
    )
    .unwrap();
    assert!(arm.sandbox_artifacts().is_err());
    fs::write(
        arm.package_manifest(Guest::Alpine),
        r#"{"architecture":"aarch64"}"#,
    )
    .unwrap();
    let artifacts = arm.sandbox_artifacts().unwrap();
    assert_eq!(artifacts.kernel, arm.kernel());
    assert_eq!(artifacts.initrd, arm.initramfs(Guest::Alpine));
    #[cfg(any(target_os = "linux", windows))]
    {
        artifacts.probe().unwrap();
        assert!(artifacts.runtime_identity().is_err());
        let replacement = root.path().join("foreign-artifact");
        fs::write(&replacement, b"foreign artifact").unwrap();
        for description in ["OpenVMM executable", "guest kernel", "guest initramfs"] {
            let mut replaced = artifacts.clone();
            let path = match description {
                "OpenVMM executable" => &mut replaced.openvmm,
                "guest kernel" => &mut replaced.kernel,
                _ => &mut replaced.initrd,
            };
            *path = replacement.clone();
            let error = replaced.probe().unwrap_err();
            assert_eq!(error.code(), crate::ErrorCode::Unavailable);
            assert_eq!(
                error.message(),
                format!(
                    "the {description} {} is not the one of {}, whose install metadata \
                     identifies the artifacts",
                    replacement.display(),
                    arm.root().display()
                )
            );
        }
        fs::remove_file(arm.kernel()).unwrap();
        assert_eq!(
            artifacts.probe().unwrap_err().code(),
            crate::ErrorCode::ArtifactMissing
        );
    }
    fs::write(arm.kernel(), b"foreign kernel").unwrap();
    assert!(arm.sandbox_artifacts().is_err());
}

#[test]
fn missing_arm_image_does_not_fall_back_to_existing_x86_artifacts() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("build")).unwrap();
    fs::write(root.path().join("build").join("vmlinux"), b"x86").unwrap();
    let arm = ArtifactDir::repository(root.path())
        .unwrap()
        .with_architecture(Architecture::Aarch64)
        .unwrap();
    assert!(
        arm.require_kernel()
            .unwrap_err()
            .to_string()
            .contains("Image")
    );
}

#[test]
fn arm_image_admission_matches_producer_flags_footprint_and_offset_rules() {
    let root = tempfile::tempdir().unwrap();
    let arm = ArtifactDir::repository(root.path())
        .unwrap()
        .with_architecture(Architecture::Aarch64)
        .unwrap();
    fs::create_dir_all(arm.kernel().parent().unwrap()).unwrap();
    fs::write(arm.kernel(), image()).unwrap();
    assert_eq!(arm.require_kernel().unwrap(), arm.kernel());
    for (offset, value) in [
        (24, 11_u64),
        (24, 8),
        (24, 2),
        (24, 26),
        (8, 1),
        (8, u64::MAX - 4095),
        (16, 63),
    ] {
        let mut invalid = image();
        invalid[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        fs::write(arm.kernel(), invalid).unwrap();
        assert!(arm.require_kernel().is_err(), "{offset}: {value}");
    }
    fs::write(arm.kernel(), b"\x7fELF").unwrap();
    assert!(arm.require_kernel().is_err());
    let mut wrong_magic = image();
    wrong_magic[56] = 0;
    fs::write(arm.kernel(), wrong_magic).unwrap();
    assert!(arm.require_kernel().is_err());
}

#[test]
fn arm_guest_admission_requires_matching_package_manifest_architecture() {
    let root = tempfile::tempdir().unwrap();
    let arm = ArtifactDir::repository(root.path())
        .unwrap()
        .with_architecture(Architecture::Aarch64)
        .unwrap();
    fs::create_dir_all(arm.initramfs(Guest::Alpine).parent().unwrap()).unwrap();
    fs::write(arm.initramfs(Guest::Alpine), b"initramfs fixture").unwrap();
    assert!(arm.require_initramfs(Guest::Alpine).is_err());
    fs::write(
        arm.package_manifest(Guest::Alpine),
        r#"{"architecture":"x86_64"}"#,
    )
    .unwrap();
    assert!(arm.require_initramfs(Guest::Alpine).is_err());
    fs::write(
        arm.package_manifest(Guest::Alpine),
        r#"{"architecture":"aarch64"}"#,
    )
    .unwrap();
    assert_eq!(
        arm.require_initramfs(Guest::Alpine).unwrap(),
        arm.initramfs(Guest::Alpine)
    );
    assert!(arm.require_initramfs(Guest::AzureLinux).is_err());
    fs::write(arm.initramfs(Guest::Ubuntu), b"Ubuntu initramfs fixture").unwrap();
    fs::write(
        arm.package_manifest(Guest::Ubuntu),
        r#"{"architecture":"amd64"}"#,
    )
    .unwrap();
    assert!(arm.require_initramfs(Guest::Ubuntu).is_err());
    fs::write(
        arm.package_manifest(Guest::Ubuntu),
        r#"{"architecture":"arm64"}"#,
    )
    .unwrap();
    assert_eq!(
        arm.require_initramfs(Guest::Ubuntu).unwrap(),
        arm.initramfs(Guest::Ubuntu)
    );
}

#[test]
fn arm_release_selection_fails_closed_instead_of_inventing_a_catalog_entry() {
    let root = tempfile::tempdir().unwrap();
    assert!(
        ArtifactDir::release(root.path())
            .unwrap()
            .with_architecture(Architecture::Aarch64)
            .is_err()
    );
}
