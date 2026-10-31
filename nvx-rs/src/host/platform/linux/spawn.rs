//! The spawns of OpenVMM, which pass it no descriptor of the caller but those that they list.

use std::io;
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::process::Command;

use crate::host::platform::inherit::Inherited;
use crate::host::sys;

/// Starts the command in a new session so it outlives the caller and its terminal, with the
/// descriptors `inherit` under their numbers in the caller, and no other descriptor of the caller
/// beyond the standard streams that the command sets up, and with the caller's umask without the
/// owner's bits, so that the files that the command creates for the caller keep the owner's
/// permissions that it gives them, whatever the caller's umask, outside directories with a default
/// ACL, which Linux applies instead of the umask.
///
/// Only the new process clears the close-on-exec flag of the descriptors in `inherit`, and changes
/// its umask, so no process that another thread spawns meanwhile inherits the descriptors, and the
/// caller keeps its umask. Fails with [`io::ErrorKind::InvalidInput`] for a descriptor of a
/// standard stream, 0 to 2, which the command's own standard streams replace in the new process,
/// and for a descriptor that is not close-on-exec in the caller, which any process that the caller
/// spawns would inherit.
pub(crate) fn detach(command: &mut Command, inherit: &[Inherited<'_>]) -> io::Result<()> {
    sys::detach(command, allowlist(inherit)?);
    Ok(())
}

/// Starts the command in the caller's session, as a child that shares the caller's terminal, with
/// the descriptors `inherit` under their numbers in the caller, no other descriptor of the caller
/// beyond the standard streams that the command sets up, and the caller's umask without the
/// owner's bits, as [`detach`] does.
///
/// Fails as [`detach`] does.
#[cfg(feature = "cli")]
pub(crate) fn inherit_only(command: &mut Command, inherit: &[Inherited<'_>]) -> io::Result<()> {
    sys::inherit_only(command, allowlist(inherit)?);
    Ok(())
}

/// Returns the numbers of the descriptors `inherit`, each once, after checking that a spawn can
/// pass them to its process alone, as [`detach`] says.
fn allowlist(inherit: &[Inherited<'_>]) -> io::Result<Vec<RawFd>> {
    use nix::fcntl::{FcntlArg, FdFlag, fcntl};

    let mut descriptors: Vec<RawFd> = Vec::with_capacity(inherit.len());
    for inherited in inherit {
        let descriptor = inherited.as_fd().as_raw_fd();
        if descriptor <= sys::LAST_STANDARD_STREAM {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "descriptor {descriptor} cannot be inherited: the VMM's own standard streams \
                     replace descriptors 0 to {}",
                    sys::LAST_STANDARD_STREAM
                ),
            ));
        }
        let flags = FdFlag::from_bits_retain(fcntl(inherited.as_fd(), FcntlArg::F_GETFD)?);
        if !flags.contains(FdFlag::FD_CLOEXEC) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "descriptor {descriptor} cannot be inherited by the VMM alone: it is not \
                     close-on-exec, so any process that this process spawns inherits it"
                ),
            ));
        }
        if !descriptors.contains(&descriptor) {
            descriptors.push(descriptor);
        }
    }
    Ok(descriptors)
}
