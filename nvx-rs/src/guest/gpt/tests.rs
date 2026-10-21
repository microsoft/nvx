//! Tests of the comparison of a disk's table with the kernel's partitions, of the wait for them to
//! match, and of the reading of fake sysfs descriptions of a disk.

use std::error::Error;
use std::fs;
use std::os::unix::fs::symlink;
use std::time::{Duration, Instant};

use super::*;
use crate::gpt::GptError;
use crate::gpt::tests::standard_disk;

/// Returns the kernel's partition `number` of disk `vda`, 254:0, from byte `start` with `size`
/// bytes.
fn kernel(number: u8, start: u64, size: u64) -> KernelPartition {
    KernelPartition {
        number,
        name: OsString::from(format!("vda{number}")),
        device_number: DeviceNumber {
            major: 254,
            minor: u32::from(number),
        },
        start,
        size,
    }
}

/// Returns kernel partitions that match the partitions of `disk`.
fn matching(disk: &Disk) -> Vec<KernelPartition> {
    disk.partitions()
        .iter()
        .map(|entry| kernel(entry.number(), entry.offset(), entry.size()))
        .collect()
}

#[test]
fn matching_partitions_return_their_devices() {
    let disk = standard_disk();
    let devices = compare(&disk, &matching(&disk)).unwrap();
    assert_eq!(devices.len(), 3);
    for (device, entry) in devices.iter().zip(disk.partitions()) {
        assert_eq!(device.partition(), entry);
        assert_eq!(device.name(), format!("vda{}", entry.number()).as_str());
        assert_eq!(device.device_number().major(), 254);
        assert_eq!(device.device_number().minor(), u32::from(entry.number()));
    }
}

#[test]
fn the_first_difference_in_number_order_fails() {
    let disk = standard_disk();
    let all = matching(&disk);
    // The standard disk's partitions: p1 in LBA 34, p2 in 35 through 42, and p3 in 43 through 58.
    assert_eq!(
        (all[1].start, all[1].size, all[2].start, all[2].size),
        (35 * 512, 8 * 512, 43 * 512, 16 * 512)
    );
    assert_eq!(compare(&disk, &[]), Err(Mismatch::Missing { number: 1 }));
    let without_p2 = [all[0].clone(), all[2].clone()];
    assert_eq!(
        compare(&disk, &without_p2),
        Err(Mismatch::Missing { number: 2 })
    );
    assert_eq!(
        compare(&disk, &all[..2]),
        Err(Mismatch::Missing { number: 3 })
    );

    let mut extra = all.clone();
    extra.push(kernel(4, 59 * 512, 512));
    assert_eq!(
        compare(&disk, &extra),
        Err(Mismatch::Unexpected { number: 4 })
    );
    let mut distant = all.clone();
    distant.push(kernel(255, 59 * 512, 512));
    assert_eq!(
        compare(&disk, &distant),
        Err(Mismatch::Unexpected { number: 255 })
    );
    // A missing p2 precedes an extra p5.
    let mut both = without_p2.to_vec();
    both.push(kernel(5, 59 * 512, 512));
    assert_eq!(compare(&disk, &both), Err(Mismatch::Missing { number: 2 }));

    let mut moved = all.clone();
    moved[1].start += 512;
    assert_eq!(
        compare(&disk, &moved),
        Err(Mismatch::Extent {
            number: 2,
            start: 36 * 512,
            size: 8 * 512,
            expected_start: 35 * 512,
            expected_size: 8 * 512,
        })
    );
    let mut truncated = all.clone();
    truncated[2].size -= 512;
    assert_eq!(
        compare(&disk, &truncated),
        Err(Mismatch::Extent {
            number: 3,
            start: 43 * 512,
            size: 15 * 512,
            expected_start: 43 * 512,
            expected_size: 16 * 512,
        })
    );
}

#[test]
fn the_wait_returns_once_late_partitions_match() {
    let disk = standard_disk();
    let all = matching(&disk);
    let mut observations = 0;
    let devices = wait_for_partitions(&disk, Duration::from_secs(60), || {
        observations += 1;
        Ok(match observations {
            1 => Vec::new(),
            2 | 3 => all[..2].to_vec(),
            _ => all.clone(),
        })
    })
    .unwrap();
    assert_eq!(observations, 4);
    assert_eq!(devices, compare(&disk, &all).unwrap());
}

#[test]
fn the_wait_fails_with_the_difference_that_remains_at_the_deadline() {
    let disk = standard_disk();
    let all = matching(&disk);
    let mut extra = all.clone();
    extra.push(kernel(4, 59 * 512, 512));
    let timeout = Duration::from_millis(200);
    let start = Instant::now();
    let mut observations = 0;
    let error = wait_for_partitions(&disk, timeout, || {
        observations += 1;
        Ok(if observations == 1 {
            all[..2].to_vec()
        } else {
            extra.clone()
        })
    })
    .unwrap_err();
    assert!(
        matches!(
            error,
            DeviceError::Partitions(Mismatch::Unexpected { number: 4 })
        ),
        "{error:?}"
    );
    assert!(start.elapsed() >= timeout);
    // Pauses of 1, 2, 4, ... ms, then 50 ms: no busy wait, and more than one observation.
    assert!((3..=20).contains(&observations), "{observations}");
}

#[test]
fn a_zero_timeout_compares_once() {
    let disk = standard_disk();
    let mut observations = 0;
    let error = wait_for_partitions(&disk, Duration::ZERO, || {
        observations += 1;
        Ok(Vec::new())
    })
    .unwrap_err();
    assert!(matches!(
        error,
        DeviceError::Partitions(Mismatch::Missing { number: 1 })
    ));
    assert_eq!(observations, 1);
    let all = matching(&disk);
    assert!(wait_for_partitions(&disk, Duration::ZERO, || Ok(all.clone())).is_ok());
}

#[test]
fn a_failed_observation_ends_the_wait() {
    let disk = standard_disk();
    let mut observations = 0;
    let error = wait_for_partitions(&disk, Duration::from_secs(60), || {
        observations += 1;
        Err(malformed(Path::new("/sys/dev/block/254:0/vda1/start")))
    })
    .unwrap_err();
    assert!(matches!(error, DeviceError::Sysfs { .. }), "{error:?}");
    assert_eq!(observations, 1);
}

/// Writes the attribute `name` of the sysfs directory `directory` with `value`.
fn attribute(directory: &Path, name: &str, value: &str) {
    fs::write(directory.join(name), value).unwrap();
}

/// Returns a fake sysfs directory of disk `vda`, 254:0, with the attributes, directories, and
/// links of a disk, and no partitions.
fn disk_directory() -> tempfile::TempDir {
    let disk = tempfile::tempdir().unwrap();
    attribute(disk.path(), "dev", "254:0\n");
    attribute(disk.path(), "size", "8192\n");
    attribute(disk.path(), "ro", "1\n");
    for directory in ["holders", "mq", "power", "queue", "slaves"] {
        fs::create_dir(disk.path().join(directory)).unwrap();
    }
    symlink("../../../virtio0", disk.path().join("device")).unwrap();
    symlink("../../../../../class/block", disk.path().join("subsystem")).unwrap();
    disk
}

/// Writes the sysfs directory of partition `number` of the disk whose sysfs directory is `disk`,
/// `vda{number}`, from sector `start` with `size` sectors, and returns it.
fn partition_directory(disk: &Path, number: u8, start: u64, size: u64) -> PathBuf {
    let directory = disk.join(format!("vda{number}"));
    fs::create_dir(&directory).unwrap();
    attribute(&directory, "partition", &format!("{number}\n"));
    attribute(&directory, "start", &format!("{start}\n"));
    attribute(&directory, "size", &format!("{size}\n"));
    attribute(&directory, "dev", &format!("254:{number}\n"));
    attribute(&directory, "ro", "0\n");
    directory
}

#[test]
fn partitions_read_from_their_sysfs_directories_in_number_order() {
    let disk = disk_directory();
    assert_eq!(observe_partitions(disk.path()).unwrap(), Vec::new());
    for (number, start, size) in [(3, 43, 16), (1, 34, 1), (2, 35, 8)] {
        partition_directory(disk.path(), number, start, size);
    }
    let observed = observe_partitions(disk.path()).unwrap();
    assert_eq!(
        observed,
        [
            kernel(1, 34 * 512, 512),
            kernel(2, 35 * 512, 8 * 512),
            kernel(3, 43 * 512, 16 * 512),
        ]
    );
    assert!(compare(&standard_disk(), &observed).is_ok());
}

#[test]
fn partitions_that_vanish_while_read_are_left_out() {
    let disk = disk_directory();
    partition_directory(disk.path(), 1, 34, 1);
    for name in ["partition", "start", "size", "dev"] {
        let directory = partition_directory(disk.path(), 2, 35, 8);
        fs::remove_file(directory.join(name)).unwrap();
        assert_eq!(
            observe_partitions(disk.path()).unwrap(),
            [kernel(1, 34 * 512, 512)],
            "{name}"
        );
        fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn attributes_in_other_forms_than_the_kernel_writes_fail() {
    for (name, value) in [
        ("partition", "0\n"),
        ("partition", "256\n"),
        ("partition", "+2\n"),
        ("start", "35"),
        ("start", "35\n\n"),
        ("start", " 35\n"),
        ("start", "36028797018963968\n"),
        ("size", "eight\n"),
        ("size", "\n"),
        ("dev", "254-2\n"),
        ("dev", "254:\n"),
        ("dev", "254:2:0\n"),
        ("dev", "4294967296:2\n"),
    ] {
        let disk = disk_directory();
        let directory = partition_directory(disk.path(), 2, 35, 8);
        attribute(&directory, name, value);
        match observe_partitions(disk.path()) {
            Err(DeviceError::Sysfs { path, source }) => {
                assert_eq!(path, directory.join(name), "{value:?}");
                assert_eq!(source.kind(), io::ErrorKind::InvalidData, "{value:?}");
            }
            other => panic!("{name} {value:?}: {other:?}"),
        }
    }
}

#[test]
fn a_disk_without_a_sysfs_directory_fails() {
    let disk = disk_directory();
    let absent = disk.path().join("absent");
    match observe_partitions(&absent) {
        Err(DeviceError::Sysfs { path, source }) => {
            assert_eq!(path, absent);
            assert_eq!(source.kind(), io::ErrorKind::NotFound);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn only_block_devices_are_taken() {
    let file = tempfile::tempfile().unwrap();
    assert!(matches!(
        BlockDevice::new(&file),
        Err(DeviceError::NotBlockDevice)
    ));
    let null = File::open("/dev/null").unwrap();
    assert!(matches!(
        BlockDevice::new(&null),
        Err(DeviceError::NotBlockDevice)
    ));
}

#[test]
fn errors_describe_the_failure_and_keep_their_cause() {
    assert_eq!(
        DeviceNumber {
            major: 259,
            minor: 4
        }
        .to_string(),
        "259:4"
    );
    let error = DeviceError::NotWholeDisk;
    assert_eq!(
        error.to_string(),
        "the block device is a partition, not a whole disk"
    );
    assert!(error.source().is_none());
    let busy = io::Error::from_raw_os_error(Errno::EBUSY as i32);
    let error = DeviceError::device(DeviceOperation::RereadPartitions, busy);
    assert!(
        error
            .to_string()
            .starts_with("cannot reread the device's partition table: "),
        "{error}"
    );
    assert_eq!(
        error
            .source()
            .and_then(|source| source.downcast_ref::<io::Error>())
            .and_then(io::Error::raw_os_error),
        Some(Errno::EBUSY as i32)
    );
    let error = malformed(Path::new("/sys/dev/block/254:0/vda2/start"));
    assert_eq!(
        error.to_string(),
        "cannot read /sys/dev/block/254:0/vda2/start: unexpected text"
    );
    let error = DeviceError::from(GptError::NoProtectiveRecord);
    assert_eq!(
        error.to_string(),
        "invalid partition table: the MBR has no protective record of type 0xEE, so the disk \
         is not a GPT disk"
    );
    assert!(error.source().is_some());
    let error = DeviceError::Partitions(Mismatch::Extent {
        number: 2,
        start: 36 * 512,
        size: 8 * 512,
        expected_start: 35 * 512,
        expected_size: 8 * 512,
    });
    assert_eq!(
        error.to_string(),
        "the kernel's partitions differ from the table's: the kernel's partition 2 has 4096 \
         bytes from byte 18432, but the table's has 4096 bytes from byte 17920"
    );
    assert!(error.source().is_none());
    assert_eq!(
        Mismatch::Missing { number: 3 }.to_string(),
        "the kernel has no partition 3, which the table has"
    );
    assert_eq!(
        Mismatch::Unexpected { number: 4 }.to_string(),
        "the kernel has a partition 4, which the table lacks"
    );
}
