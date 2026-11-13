//! Safe readiness polling for owned descriptors.

use std::os::fd::BorrowedFd;
use std::time::Duration;

use nix::poll::{PollFd, PollFlags, PollTimeout};

use super::{ChildSignalWait, Errno, FileDescriptor, SignalDescriptor, Timer};

/// Polls `descriptors` for readable data or hangup, and returns whether each one woke, in the
/// caller's order.
///
/// Any reported event wakes a descriptor, matching the C agent's `revents != 0` test, so a pipe
/// whose writers all closed wakes with `POLLHUP` alone.
pub(crate) fn readable_or_hung_up(
    descriptors: &[BorrowedFd<'_>],
    timeout_milliseconds: u16,
) -> Result<Vec<bool>, Errno> {
    let descriptors: Vec<_> = descriptors
        .iter()
        .map(|&descriptor| (descriptor, false))
        .collect();
    ready(&descriptors, timeout_milliseconds)
}

/// Polls each descriptor for readable data or hangup, as [`readable_or_hung_up`] does, or, where
/// its flag is set, for space to write, and returns whether each one woke, in the caller's order.
pub(crate) fn ready(
    descriptors: &[(BorrowedFd<'_>, bool)],
    timeout_milliseconds: u16,
) -> Result<Vec<bool>, Errno> {
    let mut polls = descriptors
        .iter()
        .map(|&(descriptor, write)| {
            let interest = if write {
                PollFlags::POLLOUT
            } else {
                PollFlags::POLLIN | PollFlags::POLLHUP
            };
            PollFd::new(descriptor, interest)
        })
        .collect::<Vec<_>>();
    nix::poll::poll(&mut polls, timeout_milliseconds)?;
    Ok(polls
        .iter()
        .map(|poll| poll.revents().is_some_and(|events| !events.is_empty()))
        .collect())
}

/// A descriptor owner and the readiness that [`poll`] waits for.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Pollable<'a> {
    /// A file, device, pipe, or socket.
    File(&'a FileDescriptor),
    /// A file, device, pipe, or socket with space to write.
    Writable(&'a FileDescriptor),
    /// A timer, which polls readable once it expires.
    Timer(&'a Timer),
    /// A signal descriptor, which polls readable while one of its signals is pending.
    Signals(&'a SignalDescriptor),
    /// A child-signal wait, which polls readable while `SIGCHLD` is pending.
    ChildSignal(&'a ChildSignalWait),
}

impl Pollable<'_> {
    /// Borrows the owner's descriptor.
    fn borrowed(&self) -> BorrowedFd<'_> {
        match self {
            Self::File(descriptor) | Self::Writable(descriptor) => descriptor.borrowed(),
            Self::Timer(timer) => timer.borrowed(),
            Self::Signals(signals) => signals.borrowed(),
            Self::ChildSignal(wait) => wait.borrowed(),
        }
    }
}

/// The events that [`poll`] reports for one descriptor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PollEvents {
    /// The descriptor has input to read.
    pub(crate) readable: bool,
    /// The descriptor has space to write.
    pub(crate) writable: bool,
    /// The descriptor has an error condition, which its next I/O returns.
    pub(crate) error: bool,
}

/// Waits at most `timeout`, or without a limit for `None`, until one of `descriptors` has its
/// requested readiness or an error, with one `poll` call and without allocating, and returns their
/// events in the caller's order. An interrupted call is `EINTR`, not a retry.
///
/// A timeout of more than `i32::MAX` milliseconds waits for `i32::MAX` milliseconds, about 24
/// days.
pub(crate) fn poll<const N: usize>(
    descriptors: [Pollable<'_>; N],
    timeout: Option<Duration>,
) -> Result<[PollEvents; N], Errno> {
    let timeout = timeout.map_or(PollTimeout::NONE, |timeout| {
        PollTimeout::try_from(timeout.as_millis()).unwrap_or(PollTimeout::MAX)
    });
    let mut raw = descriptors.each_ref().map(|descriptor| {
        let interest = match descriptor {
            Pollable::Writable(_) => PollFlags::POLLOUT,
            _ => PollFlags::POLLIN,
        };
        PollFd::new(descriptor.borrowed(), interest)
    });
    nix::poll::poll(&mut raw, timeout)?;
    Ok(raw.map(|descriptor| {
        let returned = descriptor.revents().unwrap_or(PollFlags::empty());
        PollEvents {
            readable: returned.contains(PollFlags::POLLIN),
            writable: returned.contains(PollFlags::POLLOUT),
            error: returned.contains(PollFlags::POLLERR),
        }
    }))
}

#[cfg(test)]
mod tests;
