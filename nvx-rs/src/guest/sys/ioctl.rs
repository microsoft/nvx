//! Safe, operation-specific wrappers for the Linux ioctls of the guest tools.

use std::ffi::c_int;
use std::mem::size_of;
use std::os::fd::{AsRawFd, BorrowedFd};

use super::{Errno, FileDescriptor};

/// Number of bytes that `nvx-reseed` adds to the entropy pool.
const SEED_BYTES: usize = 64;

/// C layout required by `RNDADDENTROPY`, whose copy of the seed it zeroes when it is dropped.
#[repr(C)]
struct SeedPayload {
    /// Number of entropy bits credited to the payload.
    entropy_count: c_int,
    /// Number of following seed bytes.
    buf_size: c_int,
    /// Seed bytes added to the kernel pool.
    bytes: [u8; SEED_BYTES],
}

impl Drop for SeedPayload {
    /// Overwrites the payload's copy of the seed with zeros, with volatile writes.
    fn drop(&mut self) {
        super::zeroize(&mut self.bytes);
    }
}

nix::ioctl_read_bad!(
    /// Reads the number of bytes waiting to be read: Linux `FIONREAD`, whose number `nix`
    /// takes from its `libc` dependency because it defines no constant of its own.
    fionread,
    nix::libc::FIONREAD,
    c_int
);

nix::ioctl_write_ptr_bad!(
    /// Adds entropy to the kernel pool: Linux `RNDADDENTROPY`, `_IOW('R', 0x03, int[2])`.
    rnd_add_entropy,
    nix::request_code_write!(b'R', 0x03, size_of::<[c_int; 2]>()),
    SeedPayload
);

nix::ioctl_none!(
    /// Reseeds the kernel's CRNG: Linux `RNDRESEEDCRNG`, `_IO('R', 0x07)`.
    rnd_reseed_crng,
    b'R',
    0x07
);

nix::ioctl_none!(
    /// Writes a block device's dirty buffers and drops its cached ones: Linux `BLKFLSBUF`,
    /// `_IO(0x12, 97)`.
    blkflsbuf,
    0x12,
    97
);

nix::ioctl_none!(
    /// Rereads a disk's partition table: Linux `BLKRRPART`, `_IO(0x12, 95)`.
    blkrrpart,
    0x12,
    95
);

nix::ioctl_read_bad!(
    /// Reads a block device's logical block size: Linux `BLKSSZGET`, `_IO(0x12, 104)`, which
    /// writes an `int` although its number declares no argument.
    blksszget,
    nix::request_code_none!(0x12, 104),
    c_int
);

nix::ioctl_read_bad!(
    /// Reads a block device's size in bytes: Linux `BLKGETSIZE64`, `_IOR(0x12, 114, size_t)`,
    /// which writes a 64-bit size whatever the size of `size_t`.
    blkgetsize64,
    nix::request_code_read!(0x12, 114, size_of::<usize>()),
    u64
);

/// Returns the number of bytes available for reading from `descriptor`.
///
/// The local integer outlives `ioctl`, and the wrapper returns it only after the kernel reports
/// success, so callers never handle the raw pointer or descriptor.
pub(crate) fn console_pending(descriptor: &FileDescriptor) -> Result<c_int, Errno> {
    let mut pending = 0;
    // SAFETY: `pending` is writable for one C integer and outlives the `FIONREAD` call.
    unsafe { fionread(descriptor.raw(), &raw mut pending) }?;
    Ok(pending)
}

/// Adds a 64-byte seed with full entropy credit and asks Linux to reseed the CRNG.
///
/// The payload owns the exact C layout consumed synchronously by `RNDADDENTROPY`, and a copy of
/// the seed, which it zeroes when the call returns. `RNDRESEEDCRNG` takes no argument. If either
/// request fails, the error of the failing request is returned exactly as the C `||` expression
/// did.
pub(crate) fn reseed_random(
    descriptor: &FileDescriptor,
    seed: &[u8; SEED_BYTES],
) -> Result<(), Errno> {
    let mut payload = SeedPayload {
        entropy_count: (SEED_BYTES * 8) as c_int,
        buf_size: SEED_BYTES as c_int,
        bytes: [0; SEED_BYTES],
    };
    payload.bytes.copy_from_slice(seed);
    // SAFETY: `payload` has the kernel ABI's layout and outlives this synchronous ioctl.
    unsafe { rnd_add_entropy(descriptor.raw(), &raw const payload) }?;
    // SAFETY: `RNDRESEEDCRNG` reads no argument.
    unsafe { rnd_reseed_crng(descriptor.raw()) }?;
    Ok(())
}

/// Writes the dirty buffers of the block device that `descriptor` opened, then drops its cached
/// buffers, as `blockdev --flushbufs` does, so that later reads come from the device. It needs
/// `CAP_SYS_ADMIN`.
pub(crate) fn flush_block_buffers(descriptor: &FileDescriptor) -> Result<(), Errno> {
    // SAFETY: `BLKFLSBUF` reads no argument.
    unsafe { blkflsbuf(descriptor.raw()) }?;
    Ok(())
}

/// Asks Linux to reread the partition table of the disk whose whole-disk block device
/// `descriptor` opened, as `blockdev --rereadpt` does: the kernel drops the disk's cached buffers
/// and the block devices of its partitions, reads the table from the device, and creates the
/// block devices of the partitions that it finds, whose nodes devtmpfs creates, before the request
/// returns. It needs `CAP_SYS_ADMIN`, and fails with `EBUSY` while one of the disk's partitions is
/// open, as a mounted one is, and with `EINVAL` on a partition, or on a disk that the kernel does
/// not partition.
pub(crate) fn reread_partition_table(descriptor: &FileDescriptor) -> Result<(), Errno> {
    // SAFETY: `BLKRRPART` reads no argument.
    unsafe { blkrrpart(descriptor.raw()) }?;
    Ok(())
}

/// Returns the logical block size, in bytes, of the block device that `descriptor` opened, as
/// `blockdev --getss` prints it.
///
/// It borrows the descriptor, which it neither reads nor writes, and whose file position it
/// leaves alone, so that a caller may ask this of a device that the standard library opened.
pub(crate) fn logical_block_size(descriptor: BorrowedFd<'_>) -> Result<u32, Errno> {
    let mut size: c_int = 0;
    // SAFETY: `size` is writable for one C `int` and outlives the synchronous `BLKSSZGET`.
    unsafe { blksszget(descriptor.as_raw_fd(), &raw mut size) }?;
    u32::try_from(size).map_err(|_| Errno::EINVAL)
}

/// Returns the size, in bytes, of the block device that `descriptor` opened, as
/// `blockdev --getsize64` prints it.
///
/// It borrows the descriptor as `logical_block_size` does.
pub(crate) fn block_device_size(descriptor: BorrowedFd<'_>) -> Result<u64, Errno> {
    let mut size: u64 = 0;
    // SAFETY: `size` is writable for one `u64` and outlives the synchronous `BLKGETSIZE64`.
    unsafe { blkgetsize64(descriptor.as_raw_fd(), &raw mut size) }?;
    Ok(size)
}

#[cfg(test)]
mod tests {
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
}
