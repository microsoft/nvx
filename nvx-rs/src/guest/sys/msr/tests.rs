//! Checks of the MSR device's path and the errors of a missing device.

use super::*;

#[test]
fn a_cpu_that_does_not_exist_has_no_msr_device() {
    assert_eq!(MsrFile::path(3), "/dev/cpu/3/msr");
    let error = MsrFile::open(usize::MAX, MsrAccess::Read).unwrap_err();
    assert!(matches!(error, Errno::ENOENT | Errno::ENXIO), "{error:?}");
}
