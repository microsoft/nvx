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
    let mut polls = descriptors
        .iter()
        .map(|descriptor| PollFd::new(*descriptor, PollFlags::POLLIN | PollFlags::POLLHUP))
        .collect::<Vec<_>>();
    nix::poll::poll(&mut polls, timeout_milliseconds)?;
    Ok(polls
        .iter()
        .map(|poll| poll.revents().is_some_and(|events| !events.is_empty()))
        .collect())
}

/// Returns whether `descriptor` hung up, without waiting: for the read end of a pipe, whether
/// every writer closed it, whatever bytes it still holds.
pub(crate) fn hung_up(descriptor: BorrowedFd<'_>) -> Result<bool, Errno> {
    let mut polls = [PollFd::new(descriptor, PollFlags::empty())];
    nix::poll::poll(&mut polls, PollTimeout::ZERO)?;
    Ok(polls[0]
        .revents()
        .is_some_and(|events| events.contains(PollFlags::POLLHUP)))
}

/// A descriptor owner that [`poll`] waits on for input.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Pollable<'a> {
    /// A file, device, pipe, or socket.
    File(&'a FileDescriptor),
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
            Self::File(descriptor) => descriptor.borrowed(),
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
    /// The descriptor has an error condition, which a read returns.
    pub(crate) error: bool,
}

/// Waits at most `timeout`, or without a limit for `None`, until one of `descriptors` has input
/// or an error, with one `poll` call and without allocating, and returns each descriptor's
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
    let mut raw = descriptors
        .each_ref()
        .map(|descriptor| PollFd::new(descriptor.borrowed(), PollFlags::POLLIN));
    nix::poll::poll(&mut raw, timeout)?;
    Ok(raw.map(|descriptor| {
        let returned = descriptor.revents().unwrap_or(PollFlags::empty());
        PollEvents {
            readable: returned.contains(PollFlags::POLLIN),
            error: returned.contains(PollFlags::POLLERR),
        }
    }))
}

#[cfg(test)]
mod tests;
