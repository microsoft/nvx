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

/// Sets the soft limit of the process's open descriptors to `limit`, and returns the limit that
/// it replaced, for a test that runs out of descriptors in a process of its own.
///
/// The crate does not enable `nix`'s resource limits, so this calls `getrlimit` and `setrlimit`
/// through libc.
pub(crate) fn set_descriptor_limit(limit: nix::libc::rlim_t) -> Result<nix::libc::rlim_t, Errno> {
    let mut limits = nix::libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limits` is a writable `rlimit`, which the call only fills.
    Errno::result(unsafe { nix::libc::getrlimit(nix::libc::RLIMIT_NOFILE, &mut limits) })?;
    let previous = limits.rlim_cur;
    limits.rlim_cur = limit;
    // SAFETY: `limits` is a valid `rlimit`, which the call only reads.
    Errno::result(unsafe { nix::libc::setrlimit(nix::libc::RLIMIT_NOFILE, &limits) })?;
    Ok(previous)
}

/// Creates `path` as a FIFO with `mode`, with which tests stand for a console, and keep their
/// system calls in this module.
pub(crate) fn create_fifo(path: &CStr, mode: Mode) -> Result<(), Errno> {
    nix::unistd::mkfifo(path, mode)
}
