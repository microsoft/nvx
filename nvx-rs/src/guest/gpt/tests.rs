//! Tests of the comparison of a disk's table with the kernel's partitions, of the wait for them to
//! match, and of the reading and confirmation of fake sysfs descriptions of a disk.

use std::error::Error;
use std::fs;
use std::os::unix::ffi::OsStrExt;
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
        Ok(Some(match observations {
            1 => Vec::new(),
            2 | 3 => all[..2].to_vec(),
            _ => all.clone(),
        }))
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
        Ok(Some(if observations == 1 {
            all[..2].to_vec()
        } else {
            extra.clone()
        }))
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
        Ok(Some(Vec::new()))
    })
    .unwrap_err();
    assert!(matches!(
        error,
        DeviceError::Partitions(Mismatch::Missing { number: 1 })
    ));
    assert_eq!(observations, 1);
    let all = matching(&disk);
    assert!(wait_for_partitions(&disk, Duration::ZERO, || Ok(Some(all.clone()))).is_ok());
}

#[test]
fn partitions_that_change_while_read_are_read_again() {
    let disk = standard_disk();
    let all = matching(&disk);
    let mut observations = 0;
    let devices = wait_for_partitions(&disk, Duration::from_secs(60), || {
        observations += 1;
        Ok((observations > 2).then(|| all.clone()))
    })
    .unwrap();
    assert_eq!(observations, 3);
    assert_eq!(devices, compare(&disk, &all).unwrap());
    // Partitions that are still changing at the deadline fail as such.
    let error = wait_for_partitions(&disk, Duration::ZERO, || Ok(None)).unwrap_err();
    assert!(
        matches!(error, DeviceError::Partitions(Mismatch::Changed)),
        "{error:?}"
    );
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

/// Replaces the partitions of the disk whose sysfs directory is `disk` with `partitions`, each a
/// number, a first sector, and a number of sectors, as the kernel does when it rereads the disk's
/// table: it drops every partition, then creates the table's.
fn reread(disk: &Path, partitions: &[(u8, u64, u64)]) {
    for entry in fs::read_dir(disk).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name().as_bytes().starts_with(b"vda") {
            fs::remove_dir_all(entry.path()).unwrap();
        }
    }
    for &(number, start, size) in partitions {
        partition_directory(disk, number, start, size);
    }
}

/// Observes the partitions of the disk whose sysfs directory is `disk`, which don't change while
/// they're read.
fn observe(disk: &Path) -> Vec<KernelPartition> {
    observe_partitions(disk)
        .unwrap()
        .expect("the partitions changed while they were read")
}

#[test]
fn partitions_read_from_their_sysfs_directories_in_number_order() {
    let disk = disk_directory();
    assert_eq!(observe(disk.path()), Vec::new());
    for (number, start, size) in [(3, 43, 16), (1, 34, 1), (2, 35, 8)] {
        partition_directory(disk.path(), number, start, size);
    }
    let observed = observe(disk.path());
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
        assert_eq!(observe(disk.path()), [kernel(1, 34 * 512, 512)], "{name}");
        fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn a_partition_that_the_kernel_creates_again_is_never_read_in_parts() {
    let disk = disk_directory();
    partition_directory(disk.path(), 1, 34, 1);
    partition_directory(disk.path(), 3, 43, 16);
    // The table's p2 runs from sector 35 for 8 sectors. The kernel's first p2 starts there but is
    // shorter, and the p2 that replaces it has that size but starts later, so a start read from
    // the first and a size read from the second would match the table.
    let path = partition_directory(disk.path(), 2, 35, 4);
    let directory = SysfsDirectory::open(path.clone()).unwrap().unwrap();
    assert_eq!(directory.bytes(c"start").unwrap(), Some(35 * 512));
    fs::remove_dir_all(&path).unwrap();
    partition_directory(disk.path(), 2, 36, 8);
    assert_eq!(directory.bytes(c"size").unwrap(), None);
    assert_eq!(directory.partition(OsString::from("vda2")).unwrap(), None);
    // A new observation reads the second p2 whole, and it differs from the table.
    let observed = observe(disk.path());
    assert_eq!(observed[1], kernel(2, 36 * 512, 8 * 512));
    assert_eq!(
        compare(&standard_disk(), &observed).unwrap_err(),
        Mismatch::Extent {
            number: 2,
            start: 36 * 512,
            size: 8 * 512,
            expected_start: 35 * 512,
            expected_size: 8 * 512,
        }
    );
}

/// Reads the subdirectories of the disk whose sysfs directory is `disk` as a reread tears them: the
/// disk's other directories, and p1 from an old table, which moved p2, then p2 and p3 from a new
/// table, which moves p1. Neither table matches the standard disk's, but the partitions read do.
fn read_torn(disk: &Path) -> Vec<Subdirectory> {
    // The standard disk's partitions: p1 in LBA 34, p2 in 35 through 42, and p3 in 43 through 58.
    reread(disk, &[(1, 34, 1), (2, 36, 8), (3, 43, 16)]);
    let mut torn = Vec::new();
    for name in list_subdirectories(disk).unwrap() {
        if name == "vda2" {
            reread(disk, &[(1, 60, 1), (2, 35, 8), (3, 43, 16)]);
        }
        torn.extend(Subdirectory::read(disk, name).unwrap());
    }
    let partitions: Vec<_> = torn
        .iter()
        .filter_map(|subdirectory| subdirectory.partition.clone())
        .collect();
    assert!(compare(&standard_disk(), &partitions).is_ok());
    torn
}

#[test]
fn partitions_from_before_and_after_a_reread_are_never_combined() {
    let disk = disk_directory();
    let torn = read_torn(disk.path());
    assert_eq!(confirm_partitions(disk.path(), torn).unwrap(), None);
    // The next observation reads the new table's partitions, whose p1 differs from the table's.
    assert_eq!(
        compare(&standard_disk(), &observe(disk.path())).unwrap_err(),
        Mismatch::Extent {
            number: 1,
            start: 60 * 512,
            size: 512,
            expected_start: 34 * 512,
            expected_size: 512,
        }
    );
    // Even when the kernel's partitions come to have the descriptions that the torn reading
    // read, as those of another table could, the reading isn't confirmed, since the old table's
    // p1 no longer exists.
    let torn = read_torn(disk.path());
    fs::remove_dir_all(disk.path().join("vda1")).unwrap();
    partition_directory(disk.path(), 1, 34, 1);
    assert_eq!(confirm_partitions(disk.path(), torn).unwrap(), None);
    assert_eq!(observe(disk.path()), matching(&standard_disk()));
}

#[test]
fn a_reading_is_confirmed_only_while_the_kernel_keeps_its_partitions() {
    let table = [(1, 34, 1), (2, 35, 8), (3, 43, 16)];
    let disk = disk_directory();
    let confirmed_after = |change: &dyn Fn(&Path)| {
        reread(disk.path(), &table);
        let first = read_subdirectories(disk.path()).unwrap();
        change(disk.path());
        confirm_partitions(disk.path(), first).unwrap()
    };
    assert_eq!(confirmed_after(&|_| {}), Some(matching(&standard_disk())));
    // The kernel dropped a partition, or created one.
    let dropped = |disk: &Path| fs::remove_dir_all(disk.join("vda3")).unwrap();
    assert_eq!(confirmed_after(&dropped), None);
    let created = |disk: &Path| {
        partition_directory(disk, 4, 59, 1);
    };
    assert_eq!(confirmed_after(&created), None);
    // The kernel created the same partitions again, under the same names.
    assert_eq!(confirmed_after(&|disk| reread(disk, &table)), None);
}

#[test]
fn a_partition_that_the_kernel_finishes_creating_after_a_reading_is_not_left_out() {
    let disk = disk_directory();
    reread(disk.path(), &[(1, 34, 1), (2, 35, 8), (3, 43, 16)]);
    let attributes = [
        ("partition", "4\n"),
        ("start", "59\n"),
        ("size", "1\n"),
        ("dev", "254:4\n"),
    ];
    // The kernel creates p4's directory, then its attributes, and p4 is a partition once it has
    // them all.
    let p4 = disk.path().join("vda4");
    for created in 0..attributes.len() {
        fs::create_dir(&p4).unwrap();
        for (name, value) in &attributes[..created] {
            attribute(&p4, name, value);
        }
        // A reading that the kernel doesn't change is confirmed without p4.
        assert_eq!(
            observe(disk.path()),
            matching(&standard_disk()),
            "{created}"
        );
        // Once the kernel has created the rest of p4's attributes, a reading from before isn't
        // confirmed, and the next observation reads p4, which the table lacks.
        let first = read_subdirectories(disk.path()).unwrap();
        for (name, value) in &attributes[created..] {
            attribute(&p4, name, value);
        }
        assert_eq!(
            confirm_partitions(disk.path(), first).unwrap(),
            None,
            "{created}"
        );
        assert_eq!(
            compare(&standard_disk(), &observe(disk.path())).unwrap_err(),
            Mismatch::Unexpected { number: 4 },
            "{created}"
        );
        fs::remove_dir_all(&p4).unwrap();
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
        "the kernel's partitions do not match the table's: the kernel's partition 2 has 4096 \
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
    assert_eq!(
        DeviceError::Partitions(Mismatch::Changed).to_string(),
        "the kernel's partitions do not match the table's: they changed while they were read"
    );
}
