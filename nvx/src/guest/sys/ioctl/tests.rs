//! ABI checks for the ioctls.

use std::os::fd::AsFd;

use super::*;

#[test]
fn random_ioctl_numbers_match_linux() {
    // `_IOC(direction, type, number, size)` on x86 and most other architectures.
    let ioc = |direction: u32, number: u32, size: u32| {
        (direction << 30) | (size << 16) | (u32::from(b'R') << 8) | number
    };
    let add_entropy = nix::request_code_write!(b'R', 0x03, size_of::<[c_int; 2]>());
    assert_eq!(add_entropy as u32, ioc(1, 0x03, 2 * 4));
    assert_eq!(nix::request_code_none!(b'R', 0x07) as u32, ioc(0, 0x07, 0));
    assert_eq!(size_of::<SeedPayload>(), 8 + SEED_BYTES);
}

#[test]
fn a_seed_payload_zeroes_its_copy_of_the_seed_when_dropped() {
    let mut payload = std::mem::ManuallyDrop::new(SeedPayload {
        entropy_count: 512,
        buf_size: 64,
        bytes: [0xa5; SEED_BYTES],
    });
    // SAFETY: the payload is dropped once, and owns no resource, so its memory still holds a
    // valid payload, which the test reads, once its destructor ran.
    unsafe { std::mem::ManuallyDrop::drop(&mut payload) };
    assert_eq!(
        (payload.entropy_count, payload.buf_size, payload.bytes),
        (512, 64, [0; SEED_BYTES])
    );
}

#[test]
fn the_buffer_flush_ioctl_number_matches_linux() {
    // `BLKFLSBUF` in linux/fs.h.
    assert_eq!(nix::request_code_none!(0x12, 97) as u32, 0x1261);
}

#[test]
fn the_partition_table_reread_ioctl_number_matches_linux() {
    // `BLKRRPART` in linux/fs.h.
    assert_eq!(nix::request_code_none!(0x12, 95) as u32, 0x125f);
}

#[test]
fn a_partition_table_reread_fails_on_other_files() {
    use crate::guest::sys::{Mode, OpenFlags, open, path_c_string};

    let file = tempfile::NamedTempFile::new().unwrap();
    let path = path_c_string(file.path()).unwrap();
    let descriptor = open(&path, OpenFlags::O_RDONLY, Mode::empty()).unwrap();
    assert_eq!(reread_partition_table(&descriptor), Err(Errno::ENOTTY));
}

#[test]
fn the_block_geometry_ioctl_numbers_match_linux() {
    // `BLKSSZGET` and `BLKGETSIZE64` in linux/fs.h. The latter is `_IOR(0x12, 114, size_t)`:
    // the read direction, then the size of `size_t`.
    assert_eq!(nix::request_code_none!(0x12, 104) as u32, 0x1268);
    assert_eq!(
        nix::request_code_read!(0x12, 114, size_of::<usize>()) as u32,
        (2 << 30) | ((size_of::<usize>() as u32) << 16) | 0x1272
    );
    #[cfg(target_pointer_width = "64")]
    assert_eq!(
        nix::request_code_read!(0x12, 114, size_of::<usize>()) as u32,
        0x8008_1272
    );
}

#[test]
fn the_block_geometry_ioctls_fail_on_other_files() {
    let file = tempfile::tempfile().unwrap();
    assert_eq!(logical_block_size(file.as_fd()), Err(Errno::ENOTTY));
    assert_eq!(block_device_size(file.as_fd()), Err(Errno::ENOTTY));
}
