//! The block devices that the VMM attaches in the root's roles, which the base addresses of their
//! MMIO windows name, and the block devices of a GPT disk's partitions, whose table a clone asks
//! the kernel to reread.
//!
//! The VMM declares each device on the kernel command line, so the kernel names its MMIO window
//! `virtio-mmio.N` in `/proc/iomem`, and creates its block device below the window's device in
//! `/sys/devices/virtio-mmio-cmdline`.

use std::fs::{self, File};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::kernel::{Kernel, Paths};
use super::{Device, DeviceStep, Error, Role};
use crate::guest::parse::{parse_decimal, parse_hexadecimal};
use crate::guest::sys;
use crate::uuid::Uuid;

/// The interval between two polls for a block device that has not appeared yet.
pub(super) const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// The prefix of the name of a virtio MMIO window in `/proc/iomem`.
const WINDOW_PREFIX: &[u8] = b"virtio-mmio.";
/// The directory, below sysfs, of the virtio MMIO devices that the kernel command line declares.
const WINDOW_DEVICES: &str = "devices/virtio-mmio-cmdline";
/// The byte offset of the EROFS superblock on its device.
const EROFS_SUPERBLOCK: u64 = 1024;
/// The EROFS superblock's magic number, which it starts with, little-endian.
const EROFS_MAGIC: u32 = 0xE0F5_E1E2;
/// The range of the file system's UUID in the EROFS superblock.
const EROFS_UUID: std::ops::Range<usize> = 48..64;
/// The byte offset of the ext4 superblock on its device.
const EXT4_SUPERBLOCK: u64 = 1024;
/// The range of the ext4 superblock's magic number, a little-endian `u16`.
const EXT4_MAGIC_RANGE: std::ops::Range<usize> = 0x38..0x3A;
/// The ext4 superblock's magic number.
const EXT4_MAGIC: u16 = 0xEF53;
/// The range of the ext4 superblock's compatible feature flags, a little-endian `u32`.
const EXT4_COMPAT_RANGE: std::ops::Range<usize> = 0x5C..0x60;
/// The compatible feature flag of an ext4 file system that has a journal, `has_journal`.
const EXT4_HAS_JOURNAL: u32 = 0x0004;
/// The range of the ext4 superblock's incompatible feature flags, a little-endian `u32`.
const EXT4_INCOMPAT_RANGE: std::ops::Range<usize> = 0x60..0x64;
/// The incompatible feature flag of an ext4 file system whose journal needs a recovery,
/// `needs_recovery`, which the kernel sets while it has the file system mounted writable, and
/// clears once it unmounts it cleanly.
const EXT4_NEEDS_RECOVERY: u32 = 0x0004;

/// The features of an ext4 file system that the root checks, from its superblock's feature
/// flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Ext4Features {
    /// Whether the file system has a journal.
    pub(super) journal: bool,
    /// Whether its journal needs a recovery.
    pub(super) recovery: bool,
}

/// Finds the block device of `role` that the VMM attached at `device`'s MMIO window, polling
/// until it appears for as long as `timeout` allows, but never past `deadline`, if any, checks
/// that its access mode is the role's, read-only for the distro and writable for the scratch,
/// checks its size if the caller expects one, and flushes its buffers, so that later reads come
/// from the device. Returns the path of its node.
pub(super) fn prepare(
    kernel: &mut dyn Kernel,
    paths: &Paths,
    role: Role,
    device: Device,
    timeout: Duration,
    deadline: Option<Instant>,
) -> Result<PathBuf, Error> {
    let (name, path) = resolve(kernel, paths, role, device.mmio, timeout, deadline)?;
    let attributes = paths.sys.join("class/block").join(&name);
    let read_only = fs::read(attributes.join("ro"))
        .and_then(|text| match parse_count(&text)? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(sys::Errno::EPROTO.into()),
        })
        .map_err(|error| Error::Device {
            role,
            step: DeviceStep::Access,
            device: path.clone(),
            error,
        })?;
    if read_only != role.read_only() {
        return Err(Error::Access {
            role,
            device: path,
            read_only,
        });
    }
    if let Some(expected) = device.sectors {
        let actual = fs::read(attributes.join("size"))
            .and_then(|text| parse_count(&text))
            .map_err(|error| Error::Device {
                role,
                step: DeviceStep::Size,
                device: path.clone(),
                error,
            })?;
        if actual != expected {
            return Err(Error::Size {
                role,
                device: path,
                expected,
                actual,
            });
        }
    }
    let flushed = sys::path_c_string(&path).and_then(|device| kernel.flush(&device));
    flushed.map_err(|error| Error::Device {
        role,
        step: DeviceStep::Flush,
        device: path.clone(),
        error: error.into(),
    })?;
    Ok(path)
}

/// Finds the distro device that the VMM attached at `device`'s MMIO window, polling until it
/// appears for as long as `timeout` allows, but never past `deadline`, if any, as [`prepare`]
/// does, and asks the kernel to reread the partition table of the disk on it, which drops the
/// block devices of the partitions that the kernel created from the table that the disk held
/// before, and creates those of the table that it holds now, and their nodes. It checks nothing
/// else of the device, whose checks [`prepare`] makes. Returns the path of its node.
pub(super) fn reread(
    kernel: &mut dyn Kernel,
    paths: &Paths,
    device: Device,
    timeout: Duration,
    deadline: Option<Instant>,
) -> Result<PathBuf, Error> {
    let (_, path) = resolve(kernel, paths, Role::Distro, device.mmio, timeout, deadline)?;
    let reread = sys::path_c_string(&path).and_then(|node| kernel.reread_partitions(&node));
    reread.map_err(|error| Error::Device {
        role: Role::Distro,
        step: DeviceStep::Reread,
        device: path.clone(),
        error: error.into(),
    })?;
    Ok(path)
}

/// Returns the name and the node of the block device that the VMM attached at `mmio`, polling
/// for it every [`POLL_INTERVAL`] while `timeout` allows another poll, or, if `deadline` comes
/// first, until the deadline, at which it polls once more, after a shorter sleep: it sleeps no
/// longer than until the deadline, and polls no more once the deadline passed, however long a poll
/// or a sleep took.
fn resolve(
    kernel: &mut dyn Kernel,
    paths: &Paths,
    role: Role,
    mmio: u64,
    timeout: Duration,
    deadline: Option<Instant>,
) -> Result<(String, PathBuf), Error> {
    let left = |deadline: Instant| deadline.saturating_duration_since(Instant::now());
    let polls = match deadline.map(left) {
        Some(left) if left < timeout => left.as_nanos().div_ceil(POLL_INTERVAL.as_nanos()) + 1,
        _ => timeout.as_millis() / POLL_INTERVAL.as_millis() + 1,
    };
    for poll in 0..polls {
        if poll > 0 {
            let interval = match deadline.map(left) {
                Some(left) if left.is_zero() => break,
                Some(left) => POLL_INTERVAL.min(left),
                None => POLL_INTERVAL,
            };
            kernel.sleep(interval);
        }
        let found = find(paths, mmio).map_err(|error| Error::Lookup { role, mmio, error })?;
        if let Some(name) = found {
            let node = paths.dev.join(&name);
            // The kernel creates the node right after the device's sysfs directory.
            if fs::symlink_metadata(&node).is_ok() {
                return Ok((name, node));
            }
        }
    }
    Err(Error::Missing { role, mmio })
}

/// Returns the node of the block device of each of the partitions `numbers` of the GPT disk whose
/// node is `disk`, in order.
///
/// The kernel created the block devices of the disk's partitions from the table that the disk
/// held when the kernel found it, or when it last reread the table, as a clone asks of it, and
/// devtmpfs created their nodes beside the disk's, under the names that the kernel gives them:
/// the disk's name and the partition's number, with a `p` between them if the disk's name ends
/// with a digit, as `vda2` and `nvme0n1p2` are. It waits for none: a partition whose node is
/// missing fails at once.
pub(super) fn partition_nodes(disk: &Path, numbers: &[u8]) -> Result<Vec<PathBuf>, Error> {
    numbers
        .iter()
        .map(|&number| {
            let node = partition_node(disk, number);
            match fs::symlink_metadata(&node) {
                Ok(_) => Ok(node),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    Err(Error::Partition { number, node })
                }
                Err(error) => Err(Error::Device {
                    role: Role::Distro,
                    step: DeviceStep::Status,
                    device: node,
                    error,
                }),
            }
        })
        .collect()
}

/// Returns the node of partition `number` of the disk whose node is `disk`, under the name that
/// the kernel gives the partition's block device.
fn partition_node(disk: &Path, number: u8) -> PathBuf {
    let disk_name = disk.file_name().unwrap_or_default();
    let mut name = disk_name.to_owned();
    if disk_name.as_bytes().last().is_some_and(u8::is_ascii_digit) {
        name.push("p");
    }
    name.push(number.to_string());
    disk.with_file_name(name)
}

/// Returns the name of the block device below the virtio MMIO window at `mmio`, or `None` while
/// the kernel lists no such window, or has not probed it yet.
fn find(paths: &Paths, mmio: u64) -> io::Result<Option<String>> {
    for window in window_directories(paths, mmio)? {
        if let Some(block) = block_device(&window)? {
            return Ok(Some(block));
        }
    }
    Ok(None)
}

/// Returns the sysfs directories of the virtio MMIO windows that the kernel lists at `mmio` in
/// `/proc/iomem`, in its order, below which it creates their virtio devices, and the block devices
/// and partitions of those. A directory may not exist yet: the kernel lists a window before it adds
/// the window's device.
pub(crate) fn window_directories(paths: &Paths, mmio: u64) -> io::Result<Vec<PathBuf>> {
    let windows = fs::read(paths.proc.join("iomem"))?;
    let mut directories = Vec::new();
    for line in windows.split(|&byte| byte == b'\n') {
        // `START-END : NAME`, indented below the window that holds it.
        let mut fields = line
            .split(|&byte| byte == b' ')
            .filter(|field| !field.is_empty());
        let (Some(range), Some(b":"), Some(name)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let start = range.split(|&byte| byte == b'-').next().unwrap_or(range);
        if parse_hexadecimal(start) != Some(mmio) || !name.starts_with(WINDOW_PREFIX) {
            continue;
        }
        let Ok(name) = std::str::from_utf8(name) else {
            continue;
        };
        directories.push(paths.sys.join(WINDOW_DEVICES).join(name));
    }
    Ok(directories)
}

/// Returns the name of the first block device, in byte order, of the first virtio device of the
/// window device `window`, or `None` while there is none.
fn block_device(window: &Path) -> io::Result<Option<String>> {
    for device in sorted_entries(window)? {
        if !device.starts_with("virtio") {
            continue;
        }
        if let Some(block) = sorted_entries(&window.join(&device).join("block"))?
            .into_iter()
            .next()
        {
            return Ok(Some(block));
        }
    }
    Ok(None)
}

/// Returns the names of the entries of `directory` that are UTF-8, in byte order, or none if it
/// does not exist yet.
fn sorted_entries(directory: &Path) -> io::Result<Vec<String>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut names = Vec::new();
    for entry in entries {
        if let Ok(name) = entry?.file_name().into_string() {
            names.push(name);
        }
    }
    names.sort_unstable();
    Ok(names)
}

/// Parses the canonical decimal count of a sysfs attribute and its newline, such as a block
/// device's size in 512-byte sectors.
fn parse_count(text: &[u8]) -> io::Result<u64> {
    text.strip_suffix(b"\n")
        .and_then(parse_decimal)
        .ok_or_else(|| io::Error::from(sys::Errno::EPROTO))
}

/// Reads the UUID of the EROFS file system on the block device `device` from its superblock, and
/// nothing else, or returns `None` if the device holds no EROFS superblock.
pub(super) fn erofs_uuid(device: &Path) -> io::Result<Option<Uuid>> {
    let mut superblock = [0; EROFS_UUID.end];
    match File::open(device)?.read_exact_at(&mut superblock, EROFS_SUPERBLOCK) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let magic = u32::from_le_bytes(superblock[..4].try_into().expect("four bytes"));
    if magic != EROFS_MAGIC {
        return Ok(None);
    }
    Ok(Some(Uuid::from_bytes(
        superblock[EROFS_UUID]
            .try_into()
            .expect("a UUID is 16 bytes"),
    )))
}

/// Reads the features of the ext4 file system on the block device `device` that the root checks,
/// from its superblock's feature flags, and reads nothing else, or returns `None` if the device
/// holds no ext4 superblock.
pub(super) fn ext4_features(device: &Path) -> io::Result<Option<Ext4Features>> {
    let mut superblock = [0; EXT4_INCOMPAT_RANGE.end];
    match File::open(device)?.read_exact_at(&mut superblock, EXT4_SUPERBLOCK) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let magic = u16::from_le_bytes(
        superblock[EXT4_MAGIC_RANGE]
            .try_into()
            .expect("the magic number is two bytes"),
    );
    if magic != EXT4_MAGIC {
        return Ok(None);
    }
    let flags = |range: std::ops::Range<usize>| {
        u32::from_le_bytes(
            superblock[range]
                .try_into()
                .expect("the feature flags are four bytes"),
        )
    };
    Ok(Some(Ext4Features {
        journal: flags(EXT4_COMPAT_RANGE) & EXT4_HAS_JOURNAL != 0,
        recovery: flags(EXT4_INCOMPAT_RANGE) & EXT4_NEEDS_RECOVERY != 0,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_canonical_decimal_counts() {
        assert_eq!(parse_count(b"2048\n").unwrap(), 2048);
        assert_eq!(parse_count(b"0\n").unwrap(), 0);
        for text in [&b"2048"[..], b"", b"\n", b" 1\n", b"0x10\n", b"1\n\n"] {
            assert!(parse_count(text).is_err(), "{}", text.escape_ascii());
        }
    }

    #[test]
    fn a_partition_s_node_has_the_name_that_the_kernel_gives_it() {
        let node = |disk: &str, number| partition_node(Path::new(disk), number);
        assert_eq!(node("/dev/vda", 2), Path::new("/dev/vda2"));
        assert_eq!(node("/dev/vdaa", 196), Path::new("/dev/vdaa196"));
        // A disk whose name ends with a digit has a `p` before the partition's number.
        assert_eq!(node("/dev/nvme0n1", 2), Path::new("/dev/nvme0n1p2"));
        assert_eq!(node("/dev/loop7", 12), Path::new("/dev/loop7p12"));
    }

    #[test]
    fn the_superblock_names_an_erofs_uuid_or_none() {
        let temporary = tempfile::tempdir().unwrap();
        let device = temporary.path().join("device");
        let uuid: [u8; 16] = std::array::from_fn(|index| index as u8 + 1);
        let mut image = vec![0u8; 4096];
        image[1024..1028].copy_from_slice(&EROFS_MAGIC.to_le_bytes());
        image[1024 + 48..1024 + 64].copy_from_slice(&uuid);
        fs::write(&device, &image).unwrap();
        assert_eq!(erofs_uuid(&device).unwrap(), Some(Uuid::from_bytes(uuid)));
        // Another magic number, or a device too short for a superblock, holds no EROFS.
        image[1024] ^= 1;
        fs::write(&device, &image).unwrap();
        assert_eq!(erofs_uuid(&device).unwrap(), None);
        fs::write(&device, &image[..1060]).unwrap();
        assert_eq!(erofs_uuid(&device).unwrap(), None);
        assert_eq!(
            erofs_uuid(&temporary.path().join("missing"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn the_superblock_tells_whether_an_ext4_journal_exists_and_needs_a_recovery() {
        let temporary = tempfile::tempdir().unwrap();
        let device = temporary.path().join("device");
        let features = |journal, recovery| Some(Ext4Features { journal, recovery });
        let mut image = vec![0u8; 2048];
        image[1024 + 0x38..1024 + 0x3A].copy_from_slice(&EXT4_MAGIC.to_le_bytes());
        // Other features, such as `dir_index` and `extent`, are neither.
        image[1024 + 0x5C..1024 + 0x60].copy_from_slice(&0x0020_u32.to_le_bytes());
        image[1024 + 0x60..1024 + 0x64].copy_from_slice(&0x0040_u32.to_le_bytes());
        fs::write(&device, &image).unwrap();
        assert_eq!(ext4_features(&device).unwrap(), features(false, false));
        image[1024 + 0x5C] |= 0x04;
        fs::write(&device, &image).unwrap();
        assert_eq!(ext4_features(&device).unwrap(), features(true, false));
        image[1024 + 0x60] |= 0x04;
        fs::write(&device, &image).unwrap();
        assert_eq!(ext4_features(&device).unwrap(), features(true, true));
        // Another magic number, or a device too short for the feature flags, holds no ext4.
        image[1024 + 0x38] ^= 1;
        fs::write(&device, &image).unwrap();
        assert_eq!(ext4_features(&device).unwrap(), None);
        image[1024 + 0x38] ^= 1;
        fs::write(&device, &image[..1024 + 0x62]).unwrap();
        assert_eq!(ext4_features(&device).unwrap(), None);
        assert_eq!(
            ext4_features(&temporary.path().join("missing"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }
}
