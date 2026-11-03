use super::*;

/// Sets the soft limit of the process's open descriptors to `limit`, and returns the limit that
/// it replaced, for a test that runs out of descriptors in a process of its own.
///
/// The crate does not enable `nix`'s resource limits, so this calls `getrlimit` and `setrlimit`
/// through libc.
pub fn set_descriptor_limit(limit: nix::libc::rlim_t) -> Result<nix::libc::rlim_t, Errno> {
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
pub fn create_fifo(path: &CStr, mode: Mode) -> Result<(), Errno> {
    nix::unistd::mkfifo(path, mode)
}

impl FileDescriptor {
    /// Returns whether this descriptor carries every seal that `seal_immutable` adds.
    pub fn is_sealed_immutable(&self) -> Result<bool, Errno> {
        let seals = fcntl(&self.0, FcntlArg::F_GET_SEALS).map(SealFlag::from_bits_retain)?;
        Ok(seals.contains(IMMUTABLE_SEALS))
    }
}
