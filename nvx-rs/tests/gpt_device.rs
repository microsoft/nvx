//! Validates GPT disks that a microVM attached as block devices, through `nvx::guest::gpt`, as
//! root in the guest: the `gpt-device` scenario of `nvx.py test-microvm` shares this test's
//! executable into the guest and runs its ignored tests there, one at a time.
//!
//! The scenario attaches two disks of the same size, and shares a directory of disk images, which
//! these environment variables name:
//!
//! - `NVX_GPT_DEVICE_DISK`: the read-only disk, which holds a GPT with a first partition and two
//!   layers;
//! - `NVX_GPT_DEVICE_DISK_GUID`: its disk GUID;
//! - `NVX_GPT_DEVICE_PARTITIONS`: the unique GUID and size in bytes of each of its partitions, in
//!   the order of their numbers, as `GUID:SIZE` separated by spaces;
//! - `NVX_GPT_DEVICE_FIRST_PARTITION`: the bytes of its first partition without the padding after
//!   them, in hexadecimal;
//! - `NVX_GPT_DEVICE_SCRATCH`: the writable disk, over whose start the tests write the images;
//! - `NVX_GPT_DEVICE_IMAGES`: the directory of the images: `three.img`, a GPT with a first
//!   partition and two layers, as on the read-only disk; `two.img` and `four.img`, the same
//!   without its third partition and with a fourth; `moved.img`, the same with its second
//!   partition 512 blocks later; and `mbr.img`, the same with an MBR in place of its protective
//!   MBR, whose first partition spans the first two of the GPT and whose second is the third.
//!
//! Every disk has 512-byte logical blocks, a first partition of 64 KiB, and layers that start with
//! `NVX-GPT-LAYER-` and their partition number.
#![cfg(all(target_os = "linux", feature = "guest"))]

use std::ffi::{OsStr, c_char, c_int, c_void};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::mem::size_of;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nvx::gpt::{Expected, ExpectedPartition, GptError};
use nvx::guest::gpt::{
    BlockDevice, DeviceError, DeviceOperation, Mismatch, PartitionDevice, ValidatedDisk,
};
use nvx::uuid::Uuid;

/// How long a test waits for partitions that must match.
const MATCH_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a test waits for partitions that must not match.
const MISMATCH_TIMEOUT: Duration = Duration::from_millis(500);

/// How long the late-partition test waits before it adds the partition that it removed.
const LATE: Duration = Duration::from_millis(300);

/// The size of the first partition of every disk, in bytes.
const FIRST_PARTITION_SIZE: usize = 64 * 1024;

/// The prefix of the first bytes of every layer, before its partition number.
const LAYER_MARKER: &str = "NVX-GPT-LAYER-";

/// Linux's `BLKPG_ADD_PARTITION`, of `<linux/blkpg.h>`.
const BLKPG_ADD_PARTITION: c_int = 1;

/// Linux's `BLKPG_DEL_PARTITION`, of `<linux/blkpg.h>`.
const BLKPG_DEL_PARTITION: c_int = 2;

/// Linux's `struct blkpg_partition`, of `<linux/blkpg.h>`.
#[repr(C)]
struct BlkpgPartition {
    /// The partition's first byte.
    start: i64,
    /// The partition's size, in bytes.
    length: i64,
    /// The partition's number.
    pno: c_int,
    /// A name that the kernel ignores.
    devname: [c_char; 64],
    /// A name that the kernel ignores.
    volname: [c_char; 64],
}

/// Linux's `struct blkpg_ioctl_arg`, of `<linux/blkpg.h>`.
#[repr(C)]
struct BlkpgIoctlArgument {
    /// The operation, such as [`BLKPG_ADD_PARTITION`].
    op: c_int,
    /// Flags that the kernel ignores.
    flags: c_int,
    /// The size of the partition that `data` points to.
    datalen: c_int,
    /// The partition.
    data: *mut c_void,
}

nix::ioctl_write_ptr_bad!(
    /// Adds or deletes a partition of a disk: Linux `BLKPG`, `_IO(0x12, 105)`.
    blkpg,
    nix::request_code_none!(0x12, 105),
    BlkpgIoctlArgument
);

/// Returns the value of the environment variable `name`, which the scenario sets.
fn variable(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!("{name} is unset: run the gpt-device scenario of nvx.py test-microvm")
    })
}

/// Returns the path of the read-only disk.
fn read_only_path() -> PathBuf {
    PathBuf::from(variable("NVX_GPT_DEVICE_DISK"))
}

/// Returns the path of the writable disk.
fn scratch_path() -> PathBuf {
    PathBuf::from(variable("NVX_GPT_DEVICE_SCRATCH"))
}

/// Returns the path of the image `name`.
fn image_path(name: &str) -> PathBuf {
    Path::new(&variable("NVX_GPT_DEVICE_IMAGES")).join(name)
}

/// Returns the values that the read-only disk meets: its disk GUID, and its partitions' unique
/// GUIDs and sizes.
fn expected() -> Expected {
    let partitions = variable("NVX_GPT_DEVICE_PARTITIONS")
        .split(' ')
        .map(|partition| {
            let (guid, size) = partition.split_once(':').unwrap();
            ExpectedPartition::new()
                .unique_guid(guid.parse().unwrap())
                .size(size.parse().unwrap())
        })
        .collect();
    Expected::new()
        .disk_guid(variable("NVX_GPT_DEVICE_DISK_GUID").parse().unwrap())
        .partitions(partitions)
}

/// Returns the bytes that `text` spells in hexadecimal.
fn decode_hex(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2), "{text}");
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}

/// Opens the writable disk for reading and writing, and writes the image `name` over its start.
fn scratch_with(name: &str) -> File {
    let disk = OpenOptions::new()
        .read(true)
        .write(true)
        .open(scratch_path())
        .unwrap();
    overwrite(&disk, name);
    disk
}

/// Writes the image `name` over the start of `disk`, and flushes it to the device, from which
/// the kernel rereads the table.
fn overwrite(disk: &File, name: &str) {
    let image = fs::read(image_path(name)).unwrap();
    disk.write_all_at(&image, 0).unwrap();
    disk.sync_all().unwrap();
}

/// Makes the kernel add partition `number` of `disk`, from byte `start` with `length` bytes,
/// with `operation` [`BLKPG_ADD_PARTITION`], or delete it, with [`BLKPG_DEL_PARTITION`], as a
/// program that manages the disk's partitions may.
fn change_partition(disk: &File, operation: c_int, number: u8, start: u64, length: u64) {
    let mut partition = BlkpgPartition {
        start: start.try_into().unwrap(),
        length: length.try_into().unwrap(),
        pno: number.into(),
        devname: [0; 64],
        volname: [0; 64],
    };
    let argument = BlkpgIoctlArgument {
        op: operation,
        flags: 0,
        datalen: size_of::<BlkpgPartition>().try_into().unwrap(),
        data: (&raw mut partition).cast(),
    };
    // SAFETY: `argument`, and the partition that it points to, outlive the synchronous ioctl,
    // which only reads them.
    unsafe { blkpg(disk.as_raw_fd(), &raw const argument) }.unwrap();
}

/// Opens the block device of `partition` in `/dev`, once devtmpfs has created its node.
fn open_partition(partition: &PartitionDevice) -> File {
    let path = Path::new("/dev").join(partition.name());
    let deadline = Instant::now() + MATCH_TIMEOUT;
    loop {
        if let Ok(status) = fs::metadata(&path)
            && status.file_type().is_block_device()
        {
            let number = partition.device_number();
            let rdev = status.rdev();
            assert_eq!(
                (nix::sys::stat::major(rdev), nix::sys::stat::minor(rdev)),
                (u64::from(number.major()), u64::from(number.minor())),
                "{path:?}"
            );
            return File::open(&path).unwrap();
        }
        assert!(Instant::now() < deadline, "{path:?} never appeared");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Asserts that `result` failed because the kernel's partitions still differed from the table's
/// by `expected` when the timeout passed.
#[track_caller]
fn assert_mismatch(result: Result<ValidatedDisk, DeviceError>, expected: Mismatch) {
    match result {
        Err(DeviceError::Partitions(mismatch)) => assert_eq!(mismatch, expected),
        other => panic!("expected {expected:?}, found {other:?}"),
    }
}

#[test]
#[ignore = "runs as root in the gpt-device scenario of nvx.py test-microvm"]
fn a_crafted_read_only_disk_validates() {
    let path = read_only_path();
    let file = File::open(&path).unwrap();
    let device = BlockDevice::new(&file).unwrap();
    assert_eq!(device.logical_block_size(), 512);
    let validated = device.validate(&expected(), MATCH_TIMEOUT).unwrap();
    let disk = validated.disk();
    assert_eq!(device.size(), disk.size());
    assert_eq!(
        validated.first_partition().partition(),
        disk.first_partition()
    );
    assert_eq!(validated.lowers().len(), 2);

    // The first partition reads as its opaque bytes, without its padding, and only within the
    // bound.
    let configuration = disk
        .read_first_partition(&file, FIRST_PARTITION_SIZE)
        .unwrap();
    assert_eq!(
        configuration,
        decode_hex(&variable("NVX_GPT_DEVICE_FIRST_PARTITION"))
    );
    assert!(std::str::from_utf8(&configuration).is_err());
    assert!(matches!(
        disk.read_first_partition(&file, FIRST_PARTITION_SIZE - 1),
        Err(GptError::FirstPartitionTooLarge { .. })
    ));

    // The partitions are the kernel's devices of the disk, and each layer's holds the layer.
    let disk_name = path.file_name().unwrap().to_str().unwrap();
    for partition in validated.partitions() {
        let number = partition.partition().number();
        assert_eq!(
            partition.name(),
            OsStr::new(&format!("{disk_name}{number}"))
        );
        let described = fs::read_to_string(format!("/sys/class/block/{disk_name}{number}/dev"));
        assert_eq!(
            described.unwrap(),
            format!("{}\n", partition.device_number())
        );
    }
    for layer in validated.lowers() {
        let mut marker = vec![0; LAYER_MARKER.len() + 1];
        open_partition(layer).read_exact(&mut marker).unwrap();
        let number = layer.partition().number();
        assert_eq!(marker, format!("{LAYER_MARKER}{number}").as_bytes());
    }
    // Nothing that the validation opened stays open: the disk validates again.
    assert_eq!(
        device.validate(&expected(), MATCH_TIMEOUT).unwrap(),
        validated
    );
}

#[test]
#[ignore = "runs as root in the gpt-device scenario of nvx.py test-microvm"]
fn a_disk_that_differs_from_the_expected_values_fails() {
    let file = File::open(read_only_path()).unwrap();
    let device = BlockDevice::new(&file).unwrap();
    let other_disk = Expected::new().disk_guid(Uuid::from_bytes([0x5A; 16]));
    assert!(matches!(
        device.validate(&other_disk, MATCH_TIMEOUT),
        Err(DeviceError::Table(GptError::DiskGuidMismatch { .. }))
    ));
    let other_size = Expected::new().partitions(vec![
        ExpectedPartition::new(),
        ExpectedPartition::new().size(512),
        ExpectedPartition::new(),
    ]);
    assert!(matches!(
        device.validate(&other_size, MATCH_TIMEOUT),
        Err(DeviceError::Table(GptError::PartitionSizeMismatch {
            number: 2,
            ..
        }))
    ));
}

#[test]
#[ignore = "runs as root in the gpt-device scenario of nvx.py test-microvm"]
fn only_a_whole_disk_validates() {
    let mut partition = read_only_path().into_os_string();
    partition.push("2");
    let partition = File::open(partition).unwrap();
    assert!(matches!(
        BlockDevice::new(&partition),
        Err(DeviceError::NotWholeDisk)
    ));
    let image = File::open(image_path("three.img")).unwrap();
    assert!(matches!(
        BlockDevice::new(&image),
        Err(DeviceError::NotBlockDevice)
    ));
}

#[test]
#[ignore = "runs as root in the gpt-device scenario of nvx.py test-microvm"]
fn a_written_disk_validates_once_its_partitions_are_reread() {
    let disk = scratch_with("three.img");
    let validated = BlockDevice::new(&disk)
        .unwrap()
        .validate(&Expected::new(), MATCH_TIMEOUT)
        .unwrap();
    assert_eq!(validated.partitions().len(), 3);
    // A device that is open for reading only validates too.
    let reader = File::open(scratch_path()).unwrap();
    let again = BlockDevice::new(&reader)
        .unwrap()
        .validate(&Expected::new(), MATCH_TIMEOUT);
    assert_eq!(again.unwrap(), validated);
}

#[test]
#[ignore = "runs as root in the gpt-device scenario of nvx.py test-microvm"]
fn late_partitions_are_awaited() {
    let disk = scratch_with("three.img");
    let device = BlockDevice::new(&disk).unwrap();
    let validated = device.validate(&Expected::new(), MATCH_TIMEOUT).unwrap();
    let second = *validated.partitions()[1].partition();
    change_partition(&disk, BLKPG_DEL_PARTITION, 2, 0, 0);
    let start = Instant::now();
    let awaited = thread::scope(|scope| {
        let adder = scope.spawn(|| {
            thread::sleep(LATE);
            change_partition(
                &disk,
                BLKPG_ADD_PARTITION,
                2,
                second.offset(),
                second.size(),
            );
        });
        let awaited = device.await_partitions(validated.disk(), MATCH_TIMEOUT);
        adder.join().unwrap();
        awaited
    });
    assert!(start.elapsed() >= LATE);
    assert_eq!(awaited.unwrap(), validated);
}

#[test]
#[ignore = "runs as root in the gpt-device scenario of nvx.py test-microvm"]
fn missing_partitions_fail_when_the_timeout_passes() {
    let disk = scratch_with("three.img");
    let device = BlockDevice::new(&disk).unwrap();
    let validated = device.validate(&Expected::new(), MATCH_TIMEOUT).unwrap();

    // The kernel dropped a partition of the table.
    change_partition(&disk, BLKPG_DEL_PARTITION, 3, 0, 0);
    let start = Instant::now();
    let awaited = device.await_partitions(validated.disk(), MISMATCH_TIMEOUT);
    assert!(start.elapsed() >= MISMATCH_TIMEOUT);
    assert_mismatch(awaited, Mismatch::Missing { number: 3 });

    // The table changed after the validation read it: the kernel rereads one without partition 3.
    overwrite(&disk, "two.img");
    device.reread_partitions().unwrap();
    assert_mismatch(
        device.await_partitions(validated.disk(), MISMATCH_TIMEOUT),
        Mismatch::Missing { number: 3 },
    );
}

#[test]
#[ignore = "runs as root in the gpt-device scenario of nvx.py test-microvm"]
fn extra_and_moved_partitions_fail() {
    let disk = scratch_with("three.img");
    let device = BlockDevice::new(&disk).unwrap();
    let table = device.read_disk(&Expected::new()).unwrap();

    overwrite(&disk, "four.img");
    device.reread_partitions().unwrap();
    assert_mismatch(
        device.await_partitions(&table, MISMATCH_TIMEOUT),
        Mismatch::Unexpected { number: 4 },
    );

    overwrite(&disk, "moved.img");
    device.reread_partitions().unwrap();
    let second = table.lowers()[0];
    assert_mismatch(
        device.await_partitions(&table, MISMATCH_TIMEOUT),
        Mismatch::Extent {
            number: 2,
            start: second.offset() + 512 * 512,
            size: second.size(),
            expected_start: second.offset(),
            expected_size: second.size(),
        },
    );
}

#[test]
#[ignore = "runs as root in the gpt-device scenario of nvx.py test-microvm"]
fn an_mbr_that_the_kernel_reads_instead_fails() {
    let disk = scratch_with("three.img");
    let device = BlockDevice::new(&disk).unwrap();
    let validated = device.validate(&Expected::new(), MATCH_TIMEOUT).unwrap();
    let table = validated.disk();

    overwrite(&disk, "mbr.img");
    // Read from the device, the disk is no GPT disk.
    assert!(matches!(
        device.validate(&Expected::new(), MATCH_TIMEOUT),
        Err(DeviceError::Table(GptError::NoProtectiveRecord))
    ));
    // The kernel reads the MBR's partitions, which differ from the table that was read before.
    device.reread_partitions().unwrap();
    let (first, second) = (table.first_partition(), table.lowers()[0]);
    assert_mismatch(
        device.await_partitions(table, MISMATCH_TIMEOUT),
        Mismatch::Extent {
            number: 1,
            start: first.offset(),
            size: second.offset() + second.size() - first.offset(),
            expected_start: first.offset(),
            expected_size: first.size(),
        },
    );
}

#[test]
#[ignore = "runs as root in the gpt-device scenario of nvx.py test-microvm"]
fn the_reread_fails_while_a_partition_is_open() {
    let disk = scratch_with("three.img");
    let device = BlockDevice::new(&disk).unwrap();
    let validated = device.validate(&Expected::new(), MATCH_TIMEOUT).unwrap();
    let layer = open_partition(&validated.lowers()[0]);
    match device.validate(&Expected::new(), MATCH_TIMEOUT) {
        Err(DeviceError::Device {
            operation: DeviceOperation::RereadPartitions,
            source,
        }) => assert_eq!(source.raw_os_error(), Some(Errno::EBUSY as i32)),
        other => panic!("{other:?}"),
    }
    drop(layer);
    device.validate(&Expected::new(), MATCH_TIMEOUT).unwrap();
}
