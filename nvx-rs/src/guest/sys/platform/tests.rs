use super::*;

#[test]
fn documented_offsets_and_access_matrix_are_exact() {
    for (register, offset) in [
        (Register::Data, 0),
        (Register::Status, 4),
        (Register::Time, 8),
        (Register::Shutdown, 12),
        (Register::Snapshot, 16),
    ] {
        assert_eq!(register.offset(), offset);
        for width in [0, 1, 2, 3, 4, 8] {
            assert_eq!(
                register.check(width, false).is_ok(),
                matches!(width, 1 | 2 | 4) && (register != Register::Status || width == 1),
                "{register:?} read {width}"
            );
            assert_eq!(
                register.check(width, true).is_ok(),
                width == 1 && !matches!(register, Register::Time | Register::Snapshot),
                "{register:?} write {width}"
            );
        }
    }
}

#[test]
fn snapshot_unavailability_is_explicit() {
    assert_eq!(Register::Snapshot.check(1, true), Err(Errno::EOPNOTSUPP));
}

#[test]
fn retained_or_active_authority_blocks_workload_spawn() {
    assert_eq!(check_released(), Ok(()));
    HELD.set(true);
    assert_eq!(check_released(), Err(Errno::EPERM));
    HELD.set(false);
    RETAINED.set(true);
    assert_eq!(check_released(), Err(Errno::EPERM));
    RETAINED.set(false);
    assert_eq!(check_released(), Ok(()));
}

#[test]
fn mapped_and_positional_transactions_have_identical_width_and_byte_order() {
    let file = tempfile::tempfile().unwrap();
    file.set_len(PAGE_SIZE as u64).unwrap();
    let descriptor = FileDescriptor::from_owned(file.into());
    assert_eq!(
        descriptor.pwrite(&[0x78, 0x56, 0x34, 0x12], Register::Data.offset()),
        Ok(4)
    );
    let mapping =
        MemoryMapping::shared_read_write(&descriptor, NonZeroUsize::new(PAGE_SIZE).unwrap(), 0)
            .unwrap();
    let mapped = Access {
        backend: Some(Backend::Mapped(mapping)),
        _thread: PhantomData,
    };
    let positional = Access {
        backend: Some(Backend::File(descriptor)),
        _thread: PhantomData,
    };
    for access in [&mapped, &positional] {
        assert_eq!(access.read_u8(Register::Data), Ok(0x78));
        assert_eq!(access.read_u16(Register::Data), Ok(0x5678));
        assert_eq!(access.read_u32(Register::Data), Ok(0x1234_5678));
        assert_eq!(access.read_u16(Register::Status), Err(Errno::EINVAL));
        assert_eq!(access.read_u32(Register::Status), Err(Errno::EINVAL));
        assert_eq!(access.write_u8(Register::Time, 1), Err(Errno::EINVAL));
        assert_eq!(
            access.write_u8(Register::Snapshot, 1),
            Err(Errno::EOPNOTSUPP)
        );
    }
    assert_eq!(mapped.write_u8(Register::Status, 0xa8), Ok(()));
    assert_eq!(positional.read_u8(Register::Status), Ok(0xa8));
    assert_eq!(positional.write_u8(Register::Data, 0xbc), Ok(()));
    assert_eq!(mapped.read_u32(Register::Data), Ok(0x1234_56bc));
    mapped.release().unwrap();
    positional.release().unwrap();
}

#[test]
fn mapping_bounds_and_natural_alignment_are_checked_before_access() {
    let file = tempfile::tempfile().unwrap();
    file.set_len(PAGE_SIZE as u64).unwrap();
    let descriptor = FileDescriptor::from_owned(file.into());
    let mapping =
        MemoryMapping::shared_read_write(&descriptor, NonZeroUsize::new(PAGE_SIZE).unwrap(), 0)
            .unwrap();
    for offset in [PAGE_SIZE, usize::MAX] {
        assert_eq!(mapping.device_read_u8(offset), Err(Errno::EINVAL));
        assert_eq!(mapping.device_write_u8(offset, 1), Err(Errno::EINVAL));
    }
    for offset in [1, PAGE_SIZE - 1, PAGE_SIZE, usize::MAX - 1, usize::MAX] {
        assert_eq!(mapping.device_read_u16(offset), Err(Errno::EINVAL));
    }
    for offset in [1, 2, PAGE_SIZE - 2, PAGE_SIZE, usize::MAX - 3, usize::MAX] {
        assert_eq!(mapping.device_read_u32(offset), Err(Errno::EINVAL));
    }
    assert_eq!(mapping.device_read_u16(PAGE_SIZE - 2), Ok(0));
    assert_eq!(mapping.device_read_u32(PAGE_SIZE - 4), Ok(0));
    mapping.unmap().unwrap();
}

#[test]
fn short_positional_reads_fail_instead_of_supplying_record_bytes() {
    let descriptor = FileDescriptor::from_owned(tempfile::tempfile().unwrap().into());
    let access = Access {
        backend: Some(Backend::File(descriptor)),
        _thread: PhantomData,
    };
    assert_eq!(access.read_u8(Register::Data), Err(Errno::EIO));
    assert_eq!(access.read_u16(Register::Data), Err(Errno::EIO));
    assert_eq!(access.read_u32(Register::Data), Err(Errno::EIO));
    access.release().unwrap();
}

#[test]
fn drop_releases_authority_after_a_rejected_transaction() {
    let descriptor = FileDescriptor::from_owned(tempfile::tempfile().unwrap().into());
    HELD.set(true);
    {
        let access = Access {
            backend: Some(Backend::File(descriptor)),
            _thread: PhantomData,
        };
        assert_eq!(check_released(), Err(Errno::EPERM));
        assert_eq!(access.write_u8(Register::Time, 1), Err(Errno::EINVAL));
    }
    assert_eq!(check_released(), Ok(()));
}
