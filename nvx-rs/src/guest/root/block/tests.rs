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
