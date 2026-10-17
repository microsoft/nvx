//! Safe operating-system interface of Unix hosts.
//!
//! The other host modules deny unsafe code. On Unix, they reach the operating system through the
//! standard library or this module, which makes its system calls through `nix` and confines the
//! unsafe ones behind wrappers that establish their ownership and async-signal-safety invariants.
//! `nix` has no wrapper for `pidfd_open`, `pidfd_send_signal`, `close_range`, or `getdents64`, so
//! those go through `nix`'s re-export of libc, as does the query of the `SIGINT` disposition that
//! precedes the CLI's handler. The Windows platform module still calls `windows-sys` directly.

#[cfg(feature = "cli")]
mod interrupt;
#[cfg(target_os = "linux")]
mod process;
#[cfg(target_os = "linux")]
mod socket;

#[cfg(target_os = "linux")]
pub(crate) use nix::errno::Errno;
use nix::unistd::{getegid, geteuid};

#[cfg(feature = "cli")]
pub(crate) use interrupt::{interrupted, watch_interrupts};
#[cfg(target_os = "linux")]
pub(crate) use process::{PidFd, detach};
#[cfg(all(test, target_os = "linux"))]
pub(crate) use process::{clear_close_on_exec, mark_listed_descriptors_on_spawn};
#[cfg(target_os = "linux")]
pub(crate) use socket::{connect_nonblocking, peer_pid};

/// Returns the effective user and group IDs of this process.
pub(crate) fn effective_ids() -> (u32, u32) {
    (geteuid().as_raw(), getegid().as_raw())
}
