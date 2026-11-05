use std::ffi::CString;

use super::*;

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
