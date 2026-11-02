//! The reads of files that are not block devices, and the errors.

use std::error::Error as _;
use std::io::Write;

use super::*;
use crate::guest::sys::Errno;

#[test]
fn a_file_that_is_not_a_block_device_fails_at_its_logical_block_size() {
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(&[0; 4096]).unwrap();
    match read_disk(&file, &Expected::new()) {
        Err(DeviceError::LogicalBlockSize(error)) => {
            assert_eq!(error.raw_os_error(), Some(Errno::ENOTTY as i32));
        }
        other => panic!("unexpected result: {other:?}"),
    }
}

#[test]
fn errors_name_what_failed_and_keep_their_source() {
    let cases = [
        (
            DeviceError::LogicalBlockSize(io::Error::other("BLKSSZGET failed")),
            "failed to read the device's logical block size: BLKSSZGET failed",
        ),
        (
            DeviceError::Size(io::Error::other("BLKGETSIZE64 failed")),
            "failed to read the device's size: BLKGETSIZE64 failed",
        ),
        (
            DeviceError::Table(GptError::MbrSignature),
            "invalid partition table: LBA 0 has no MBR signature",
        ),
    ];
    for (error, message) in cases {
        assert_eq!(error.to_string(), message);
        assert!(error.source().is_some(), "{error}");
    }
}
