use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::*;
use crate::guest::status::ExitStatus;
use crate::guest::status::tests::command_line;

/// Runs the probe with `arguments` through the device at `device`, and returns its exit
/// status, standard output, and standard error.
fn probe(device: &Path, arguments: &[&str]) -> (u8, Vec<u8>, Vec<u8>) {
    let args = command_line("nvx-kvm-probe", arguments);
    let device = CString::new(device.as_os_str().as_bytes()).unwrap();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let status = ExitStatus::from(run(&args, &device, &mut out, &mut err));
    (status as u8, out, err)
}

#[test]
fn usage_takes_no_argument() {
    let directory = tempfile::tempdir().unwrap();
    assert_eq!(
        probe(&directory.path().join("kvm"), &["--help"]),
        (2, Vec::new(), b"usage: nvx-kvm-probe\n".to_vec())
    );
}

#[test]
fn a_missing_device_fails_naming_it() {
    let directory = tempfile::tempdir().unwrap();
    let device = directory.path().join("kvm");
    let mut expected = b"open ".to_vec();
    expected.extend_from_slice(device.as_os_str().as_bytes());
    expected.extend_from_slice(b": ");
    expected.extend_from_slice(Errno::ENOENT.desc().as_bytes());
    expected.push(b'\n');
    assert_eq!(probe(&device, &[]), (1, Vec::new(), expected));
}

#[test]
fn a_device_without_the_kvm_api_fails() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let mut expected = b"KVM_GET_API_VERSION: ".to_vec();
    expected.extend_from_slice(Errno::ENOTTY.desc().as_bytes());
    expected.push(b'\n');
    assert_eq!(probe(file.path(), &[]), (1, Vec::new(), expected));
}

#[test]
fn the_reset_vector_holds_hlt_in_the_last_page_below_4_gib() {
    assert_eq!(PAGE_ADDRESS + RESET_VECTOR_OFFSET as u64, 0xffff_fff0);
    assert_eq!(PAGE_ADDRESS + PAGE_LENGTH as u64, 1 << 32);
}
