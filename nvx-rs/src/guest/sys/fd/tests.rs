use std::ffi::CString;

use super::*;
use std::os::unix::ffi::OsStrExt;

#[test]
fn a_bounded_file_lock_times_out_without_taking_or_leaking_the_lock() {
    let directory = tempfile::tempdir().unwrap();
    let path = CString::new(directory.path().join("lock").as_os_str().as_bytes()).unwrap();
    let held = lock_file(&path, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
    let deadline = Instant::now() + Duration::from_millis(20);
    assert!(matches!(
        FileLock::acquire(&path, Mode::S_IRUSR | Mode::S_IWUSR, Some(deadline)),
        Err(Errno::ETIMEDOUT)
    ));
    drop(held);
    let next = FileLock::acquire(
        &path,
        Mode::S_IRUSR | Mode::S_IWUSR,
        Some(Instant::now() + Duration::from_secs(1)),
    )
    .unwrap();
    drop(next);
    lock_file(&path, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
}

#[test]
fn an_expired_lock_deadline_does_not_create_the_lock_file() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("not-created");
    let path = CString::new(file.as_os_str().as_bytes()).unwrap();
    assert!(matches!(
        FileLock::acquire(&path, Mode::S_IRUSR | Mode::S_IWUSR, Some(Instant::now())),
        Err(Errno::ETIMEDOUT)
    ));
    assert!(!file.exists());
}

impl FileDescriptor {
    /// Formats the descriptor number for inherited-descriptor tests.
    pub(crate) fn argument(&self) -> CString {
        CString::new(self.0.as_raw_fd().to_string()).expect("a decimal descriptor has no NUL")
    }
}

#[test]
fn deadline_writes_refuse_blocking_descriptors_and_keep_legacy_writes() {
    let (reader, writer) = create_pipe().unwrap();
    assert_eq!(
        writer.write_all_until(b"bounded", Some(Instant::now() + Duration::from_secs(1))),
        Err(Errno::EINVAL)
    );
    writer.write_all_until(b"legacy", None).unwrap();
    let mut bytes = [0; 6];
    reader.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"legacy");
    let file = FileDescriptor::from_owned(tempfile::tempfile().unwrap().into());
    file.set_nonblocking().unwrap();
    assert_eq!(
        file.write_all_until(b"bounded", Some(Instant::now() + Duration::from_secs(1))),
        Err(Errno::EOPNOTSUPP)
    );
}

#[test]
fn an_expired_write_deadline_leaves_the_pipe_unchanged() {
    let (reader, writer) = create_pipe().unwrap();
    writer.set_nonblocking().unwrap();
    reader.set_nonblocking().unwrap();
    assert_eq!(
        writer.write_all_until(b"late", Some(Instant::now())),
        Err(Errno::ETIMEDOUT)
    );
    assert_eq!(reader.read(&mut [0; 4]), Err(Errno::EAGAIN));
    writer
        .write_all_until(b"on time", Some(Instant::now() + Duration::from_secs(1)))
        .unwrap();
    let mut bytes = [0; 7];
    reader.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"on time");
}

/// Creates `path` as a FIFO with `mode`, with which tests stand for a console, and keep their
/// system calls in this module.
pub(crate) fn create_fifo(path: &CStr, mode: Mode) -> Result<(), Errno> {
    nix::unistd::mkfifo(path, mode)
}
