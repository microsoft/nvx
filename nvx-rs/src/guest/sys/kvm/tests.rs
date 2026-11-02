//! ABI checks for the KVM ioctls and layouts.

use super::*;

#[test]
fn the_kvm_ioctl_numbers_match_linux() {
    // `_IO(KVMIO, nr)` and `_IOW(KVMIO, nr, size)` in linux/kvm.h, on x86.
    assert_eq!(nix::request_code_none!(KVMIO, 0x00) as u32, 0xae00);
    assert_eq!(nix::request_code_none!(KVMIO, 0x01) as u32, 0xae01);
    assert_eq!(nix::request_code_none!(KVMIO, 0x04) as u32, 0xae04);
    assert_eq!(nix::request_code_none!(KVMIO, 0x41) as u32, 0xae41);
    assert_eq!(nix::request_code_none!(KVMIO, 0x80) as u32, 0xae80);
    assert_eq!(
        nix::request_code_write!(KVMIO, 0x46, size_of::<UserspaceMemoryRegion>()) as u32,
        0x4020_ae46
    );
    assert_eq!(size_of::<UserspaceMemoryRegion>(), 32);
}

#[test]
fn opening_a_missing_device_fails() {
    let directory = tempfile::tempdir().unwrap();
    let path = super::super::path_c_string(&directory.path().join("kvm")).unwrap();
    assert_eq!(Kvm::open(&path).err(), Some(Errno::ENOENT));
}

#[test]
fn the_api_version_of_another_file_is_not_kvms() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let path = super::super::path_c_string(file.path()).unwrap();
    let kvm = Kvm::open(&path).unwrap();
    assert_eq!(kvm.api_version(), Err(Errno::ENOTTY));
    assert_eq!(kvm.create_vm().err(), Some(Errno::ENOTTY));
}
