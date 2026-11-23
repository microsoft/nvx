use super::*;
use crate::guest::sys::{FinishTimeAccess, TimeAccess, TimeWindow};
use std::os::unix::ffi::OsStrExt;

#[test]
fn an_ordinary_file_cannot_substitute_for_the_restricted_platform_device() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("platform");
    std::fs::write(&path, [0; 4096]).unwrap();
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    let descriptor = open(
        &path,
        OpenFlags::O_RDWR | OpenFlags::O_CLOEXEC,
        Mode::empty(),
    )
    .unwrap();
    assert!(matches!(
        PlatformDevice::from_descriptor(descriptor),
        Err(Errno::ENODEV)
    ));
}

#[test]
fn failed_platform_reads_are_errors_not_zero_samples() {
    let descriptor = open(
        c"/dev/null",
        OpenFlags::O_RDWR | OpenFlags::O_CLOEXEC,
        Mode::empty(),
    )
    .unwrap();
    let device: TimeAccess<'_> = PlatformDevice::from_descriptor(descriptor).unwrap();
    assert_eq!(device.status(), Err(Errno::EIO));
    assert_eq!(device.read_data_word(), Err(Errno::EIO));
    assert_eq!(device.read_time_word(), Err(Errno::EIO));
    assert_eq!(device.time_binding(), Err(Errno::EIO));
    assert_eq!(device.window_status(), Err(Errno::EIO));
    let mut sample = [0xa5; 6];
    assert_eq!(device.read_window(&mut sample), Err(Errno::EIO));
    assert_eq!(sample, [0xa5; 6]);
    device.finish().unwrap();
}

#[test]
fn failed_platform_writes_do_not_report_success() {
    let descriptor = open(
        c"/dev/full",
        OpenFlags::O_RDWR | OpenFlags::O_CLOEXEC,
        Mode::empty(),
    )
    .unwrap();
    let device = PlatformDevice::from_descriptor(descriptor).unwrap();
    assert_eq!(device.select(0xa8), Err(Errno::ENOSPC));
    assert_eq!(device.write_console(b"diagnostic"), Err(Errno::ENOSPC));
    assert_eq!(device.shutdown(193), Err(Errno::ENOSPC));
    assert_eq!(device.select_window(0xa7), Err(Errno::ENOSPC));
}

#[test]
fn clock_context_admission_requires_a_real_window_before_any_counter_read() {
    let directory = tempfile::tempdir().unwrap();
    let lock_path = directory.path().join("window.lock");
    let lock_path = std::ffi::CString::new(lock_path.as_os_str().as_bytes()).unwrap();
    let lock = FileLock::acquire(&lock_path, Mode::S_IRUSR | Mode::S_IWUSR, None).unwrap();
    let descriptor = open(
        c"/dev/zero",
        OpenFlags::O_RDWR | OpenFlags::O_CLOEXEC,
        Mode::empty(),
    )
    .unwrap();
    let device = PlatformDevice::from_descriptor(descriptor).unwrap();
    let error = device.read_clock_context(&lock).unwrap_err();
    assert!(matches!(
        error,
        ClockBindingError::Device(Errno::EOPNOTSUPP)
    ));
    assert!(error.to_string().contains("ARM platform time binding"));
    let error = device
        .read_clock_context_with_counter(&lock, || {
            panic!("no architectural counter may be read before the real window is admitted")
        })
        .unwrap_err();
    assert!(matches!(
        error,
        ClockBindingError::Device(Errno::EOPNOTSUPP)
    ));
    let invalid = ClockBindingError::Invalid(ClockError::Uninitialized);
    assert!(invalid.to_string().contains("not initialized"));
}
