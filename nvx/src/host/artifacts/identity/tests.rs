use super::*;
use crate::host::artifacts::ArtifactDir;
use crate::host::runtime::GuestArchitecture;

fn image_header() -> [u8; 64] {
    let mut bytes = [0; 64];
    bytes[8..16].copy_from_slice(&0x80000_u64.to_le_bytes());
    bytes[16..24].copy_from_slice(&0x100000_u64.to_le_bytes());
    bytes[24..32].copy_from_slice(&10_u64.to_le_bytes());
    bytes[56..60].copy_from_slice(b"ARM\x64");
    bytes
}

#[test]
fn direct_arm_image_requires_exact_flags_and_checked_footprint() {
    let valid = image_header();
    assert!(arm64_image_header(&valid, 64).is_ok());
    for (offset, value) in [
        (24, 11),
        (24, 8),
        (24, 14),
        (24, 2),
        (24, 26),
        (16, 63),
        (8, 1),
        (8, u64::MAX - 4095),
    ] {
        let mut bad = valid;
        bad[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        assert!(arm64_image_header(&bad, 64).is_err(), "{offset}: {value}");
    }
    let mut foreign = valid;
    foreign[56..60].copy_from_slice(b"\x7fELF");
    assert!(arm64_image_header(&foreign, 64).is_err());
}

#[test]
fn arm_namespace_never_falls_back_to_x86_or_relocates_native_vmm() {
    let temporary = tempfile::tempdir().unwrap();
    let x86 = ArtifactDir::repository(temporary.path()).unwrap();
    let arm = x86
        .clone()
        .with_architecture(GuestArchitecture::Aarch64)
        .unwrap();
    let build = temporary.path().join("build");
    fs::create_dir(&build).unwrap();
    fs::write(build.join("vmlinux"), b"x86-only").unwrap();
    assert_eq!(arm.kernel(), build.join("aarch64").join("Image"));
    assert_eq!(arm.openvmm(), x86.openvmm());
    assert!(arm.require_kernel().is_err());
    assert!(x86.require_kernel().is_ok());
    assert!(
        ArtifactDir::release(temporary.path())
            .unwrap()
            .with_architecture(GuestArchitecture::Aarch64)
            .is_err()
    );
}

#[test]
fn arm_release_admission_requires_matching_isa_target_and_supported_guests() {
    let temporary = tempfile::tempdir().unwrap();
    let valid = serde_json::json!({
        "distribution": {"architecture": "aarch64"},
        "nvx": {"target": "aarch64-pc-windows-msvc"},
    });
    for (field, value) in [
        ("architecture", serde_json::json!("x86_64")),
        ("architecture", serde_json::json!(null)),
        ("target", serde_json::json!("x86_64-pc-windows-msvc")),
        ("azurelinux", serde_json::json!({})),
    ] {
        let mut bad = valid.clone();
        match field {
            "architecture" => bad["distribution"][field] = value,
            "target" => bad["nvx"][field] = value,
            _ => bad[field] = value,
        }
        assert!(check_release_architecture(&bad, GuestArchitecture::Aarch64).is_err());
    }
    fs::write(
        temporary.path().join("SOURCE-MANIFEST.json"),
        valid.to_string(),
    )
    .unwrap();
    let arm = ArtifactDir::release(temporary.path())
        .unwrap()
        .with_architecture(GuestArchitecture::Aarch64)
        .unwrap();
    assert_eq!(arm.kernel(), temporary.path().join("guest").join("Image"));
    assert!(check_release_architecture(&valid, GuestArchitecture::X86_64).is_err());
    assert!(check_release_architecture(&serde_json::json!({}), GuestArchitecture::X86_64).is_ok());
}

#[cfg(windows)]
#[test]
fn native_arm_vmm_admission_rejects_foreign_and_out_of_bounds_pe_headers() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("vmm.exe");
    let mut bytes = [0; 70];
    bytes[..2].copy_from_slice(b"MZ");
    bytes[60..64].copy_from_slice(&64_u32.to_le_bytes());
    bytes[64..68].copy_from_slice(b"PE\0\0");
    for machine in [0x8664_u16, 0x14c, 0xaa64] {
        bytes[68..70].copy_from_slice(&machine.to_le_bytes());
        fs::write(&path, bytes).unwrap();
        assert_eq!(check_arm64_pe(&path).is_ok(), machine == 0xaa64);
    }
    bytes[60..64].copy_from_slice(&u32::MAX.to_le_bytes());
    fs::write(&path, bytes).unwrap();
    assert!(check_arm64_pe(&path).is_err());
    fs::write(&path, &bytes[..63]).unwrap();
    assert!(check_arm64_pe(&path).is_err());
}
