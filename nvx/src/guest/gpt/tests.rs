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

/// Compiles a disk's steps, as the module's example showed them, without opening a device: a
/// caller reads the partition table of `/dev/vda`, with the disk GUID that a boot parameter
/// names, then at most 64 KiB of configuration from its first partition.
#[test]
fn a_disk_s_steps_compile() {
    let _steps = || -> Result<usize, Box<dyn std::error::Error>> {
        let device = File::open("/dev/vda")?;
        let expected = Expected::new().disk_guid("8d2f3a4e-1c5b-4e7a-9f60-2b3c4d5e6f70".parse()?);
        let disk = read_disk(&device, &expected)?;
        let configuration = disk.read_first_partition(&device, 64 * 1024)?;
        Ok(configuration.len())
    };
}
