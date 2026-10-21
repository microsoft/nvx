//! Block devices: their geometry, the numbers that name them in sysfs, and the rereading of a
//! disk's partition table.

use std::ffi::c_int;
use std::mem::size_of;
use std::os::fd::{AsRawFd, BorrowedFd};

use super::Errno;

/// The sysfs directory that links each block device's directory under the device's number,
/// `MAJOR:MINOR`. A disk's directory holds a directory for each of its partitions, which holds a
/// `partition` file that the disk's own directory lacks.
pub(crate) const BLOCK_DEVICE_DIRECTORY: &str = "/sys/dev/block";

/// The bytes in each of the sectors in which sysfs gives a partition's `start` and `size`,
/// whatever the disk's logical block size.
pub(crate) const SYSFS_SECTOR_SIZE: u64 = 512;

nix::ioctl_read_bad!(
    /// Reads a block device's logical block size: Linux `BLKSSZGET`, `_IO(0x12, 104)`, which
    /// writes an `int` although its number declares no argument.
    get_logical_block_size,
    nix::request_code_none!(0x12, 104),
    c_int
);

nix::ioctl_read_bad!(
    /// Reads a block device's size in bytes: Linux `BLKGETSIZE64`, `_IOR(0x12, 114, size_t)`,
    /// which writes a 64-bit size whatever the size of `size_t`.
    get_size,
    nix::request_code_read!(0x12, 114, size_of::<usize>()),
    u64
);

nix::ioctl_none!(
    /// Asks the kernel to reread a disk's partition table: Linux `BLKRRPART`, `_IO(0x12, 95)`.
    reread_partition_table,
    0x12,
    95
);

/// Splits a device number, such as the `st_rdev` of a block device, into its major and minor
/// numbers.
pub(crate) fn split_device_number(device: u64) -> (u64, u64) {
    (nix::sys::stat::major(device), nix::sys::stat::minor(device))
}

/// Returns the logical block size, in bytes, of the block device that `descriptor` opens.
pub(crate) fn logical_block_size(descriptor: BorrowedFd<'_>) -> Result<u32, Errno> {
    let mut size: c_int = 0;
    // SAFETY: `size` is writable for one C `int` and outlives the synchronous `BLKSSZGET`.
    unsafe { get_logical_block_size(descriptor.as_raw_fd(), &raw mut size) }?;
    u32::try_from(size).map_err(|_| Errno::EINVAL)
}

/// Returns the size, in bytes, of the block device that `descriptor` opens.
pub(crate) fn block_device_size(descriptor: BorrowedFd<'_>) -> Result<u64, Errno> {
    let mut size = 0;
    // SAFETY: `size` is writable for one `u64` and outlives the synchronous `BLKGETSIZE64`.
    unsafe { get_size(descriptor.as_raw_fd(), &raw mut size) }?;
    Ok(size)
}

/// Asks the kernel to reread the partition table of the disk that `descriptor` opens, and to
/// create the disk's partitions again from it before it returns.
///
/// The kernel requires `CAP_SYS_ADMIN`, and fails with `EBUSY` while a partition of the disk is
/// open, with `EINVAL` for a partition or a disk that has none, and with `EIO` when it cannot
/// read the table.
pub(crate) fn reread_partitions(descriptor: BorrowedFd<'_>) -> Result<(), Errno> {
    // SAFETY: `BLKRRPART` takes no argument.
    unsafe { reread_partition_table(descriptor.as_raw_fd()) }?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! The numbers of the block-device ioctls, and the ioctls on a file that is not a block
    //! device.

    use std::os::fd::AsFd;

    use super::*;

    #[test]
    fn block_ioctl_numbers_match_linux() {
        assert_eq!(nix::request_code_none!(0x12, 104) as u32, 0x1268);
        assert_eq!(nix::request_code_none!(0x12, 95) as u32, 0x125F);
        // `_IOR(0x12, 114, size_t)`: the read direction, then the size of `size_t`.
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
    fn device_numbers_split_as_linux_encodes_them() {
        // Linux's `new_encode_dev`: the minor's low byte, the major, then the minor's high bits.
        assert_eq!(split_device_number(0xfe00), (254, 0));
        assert_eq!(split_device_number(0xfe03), (254, 3));
        assert_eq!(split_device_number(0x0101_0323), (259, 0x1023));
    }

    #[test]
    fn block_ioctls_fail_on_other_files() {
        let file = tempfile::tempfile().unwrap();
        assert_eq!(logical_block_size(file.as_fd()), Err(Errno::ENOTTY));
        assert_eq!(block_device_size(file.as_fd()), Err(Errno::ENOTTY));
        assert_eq!(reread_partitions(file.as_fd()), Err(Errno::ENOTTY));
    }
}
