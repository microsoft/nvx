//! Collecting the statuses of the processes that a guest init or agent starts.
//!
//! A process has one status collector: the caller. Nothing in the guest libraries waits for an
//! arbitrary child, which could take the status of a child that another part of the caller
//! started. A [`Child`] waits only for the child that it names, with `waitid` on the child's
//! pidfd or on its process ID, and leaves the status of every other child to the caller.
//!
//! A caller that must collect the status of every child, such as a PID 1 that adopts orphaned
//! processes, keeps its own `waitpid(-1)` loop instead, and hands each status that it collects to
//! the [`Child`] that it belongs to with [`Child::observe`]:
//!
//! ```no_run
//! use std::process::ExitStatus;
//!
//! use nvx::guest::process::Child;
//! use nvx::guest::spawn::Command;
//!
//! /// The caller's own `waitpid(-1, WNOHANG)` loop, which returns the ID and the status of one
//! /// child that has ended.
//! fn reap_any() -> Option<(u32, ExitStatus)> {
//!     # unimplemented!()
//! }
//!
//! # fn main() -> std::io::Result<()> {
//! let spawned = Command::new(c"/bin/true".to_owned(), vec![c"true".to_owned()]).spawn()?;
//! let mut child = Child::new(spawned.id())?;
//! while child.status().is_none() {
//!     if let Some((id, status)) = reap_any() {
//!         child.observe(id, status);
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Any other caller lets each [`Child`] collect its own status, for example once its
//! [pidfd](Child::pidfd) polls readable:
//!
//! ```no_run
//! use nvx::guest::process::Child;
//! use nvx::guest::spawn::Command;
//!
//! # fn main() -> std::io::Result<()> {
//! let spawned = Command::new(c"/bin/true".to_owned(), vec![c"true".to_owned()]).spawn()?;
//! let mut child = Child::with_pidfd(spawned.id())?;
//! // Poll `child.pidfd()` with the caller's other descriptors until it is readable, then:
//! let status = child.wait()?;
//! assert!(status.success());
//! # Ok(())
//! # }
//! ```

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

use super::sys::{self, Errno, ProcessId, WaitTarget};

/// A child process that the caller started and whose status it collects.
///
/// Unlike [`std::process::Child`], it neither starts the process nor holds its standard streams:
/// it names a child of the caller, for example one that [`spawn`](super::spawn) started, and
/// keeps its status once it has one. Dropping it neither waits for the child nor stops it.
///
/// Its status is the one with which it ended: an exit, or a terminating signal. A stop does not
/// end it, not even one that a wait reports to the caller as the child's tracer. A wait names the
/// child whatever signal the child reports its end with: one that `clone` started with an exit
/// signal other than `SIGCHLD`, or none, reports with that signal until it runs a program.
#[derive(Debug)]
pub struct Child {
    /// The child's process ID.
    process: ProcessId,
    /// A pidfd that refers to the child, through which waits name it.
    pidfd: Option<OwnedFd>,
    /// The status with which the child ended, once a wait or an observation collected it.
    status: Option<ExitStatus>,
}

impl Child {
    /// Names the caller's child `id`, whose status nothing has collected yet. Its waits use
    /// `waitid` on the process ID, which names the child for as long as its status stays
    /// uncollected: a caller with a collector of its own hands the child every status that
    /// the collector takes for `id`, with [`observe`](Self::observe), before the child waits, as
    /// the process ID may name another process afterwards. A pidfd keeps naming the child
    /// whatever collects its status; see [`with_pidfd`](Self::with_pidfd).
    ///
    /// An ID of 0, or one above `i32::MAX`, which no process can have, is `EINVAL`.
    pub fn new(id: u32) -> io::Result<Self> {
        Ok(Self {
            process: process_id(id)?,
            pidfd: None,
            status: None,
        })
    }

    /// Names the caller's child `id`, whose status nothing has collected yet, by a new pidfd,
    /// which [`pidfd`](Self::pidfd) lends for polling: it polls readable once the child ends.
    /// Its waits use `waitid` on the pidfd, which keeps naming the child even after another
    /// collector takes its status. The pidfd is opened at once, while the process ID still names
    /// the child.
    ///
    /// An ID of 0, or one above `i32::MAX`, is `EINVAL`, and one that names no process `ESRCH`.
    pub fn with_pidfd(id: u32) -> io::Result<Self> {
        let process = process_id(id)?;
        let pidfd = sys::open_pidfd(process)?;
        Ok(Self {
            process,
            pidfd: Some(pidfd.into()),
            status: None,
        })
    }

    /// Names the caller's child `id` by `pidfd`, a pidfd for it that the caller holds, such as the
    /// one that `clone3` returns with `CLONE_PIDFD`, which [`pidfd`](Self::pidfd) lends for
    /// polling. Its waits use `waitid` on the pidfd, as with [`with_pidfd`](Self::with_pidfd).
    /// Nothing is opened by the child's ID, so a collector of the caller's may have taken the
    /// child's status already: a wait is `ECHILD` then, and that collector hands the status over
    /// with [`observe`](Self::observe). A pidfd that its opener made nonblocking, as
    /// `PIDFD_NONBLOCK` does, makes [`wait`](Self::wait) fail with `WouldBlock` while the child
    /// runs, as `waitid` through it does; [`try_wait`](Self::try_wait) and the pidfd's polls work
    /// as with any other, and so does an execution that adopts the child, which never blocks on
    /// its status.
    ///
    /// The pidfd must refer to the process `id`, which is not checked: statuses are matched to the
    /// child by that ID, and an execution signals the process group that the child leads by that
    /// ID before Linux 6.9.
    ///
    /// An ID of 0, or one above `i32::MAX`, is `EINVAL`.
    pub fn from_pidfd(id: u32, pidfd: OwnedFd) -> io::Result<Self> {
        Ok(Self {
            process: process_id(id)?,
            pidfd: Some(pidfd),
            status: None,
        })
    }

    /// Opens a pidfd for the child by its ID, unless the child has one already or has ended: the
    /// ID names the child alone only while its status stays uncollected, which the caller
    /// guarantees until this returns.
    pub(crate) fn open_pidfd(&mut self) -> io::Result<()> {
        if self.pidfd.is_none() && self.status.is_none() {
            self.pidfd = Some(sys::open_pidfd(self.process)?.into());
        }
        Ok(())
    }

    /// Returns the child's process ID.
    pub fn id(&self) -> u32 {
        self.process.raw().unsigned_abs()
    }

    /// Returns the child's process ID for the crate's own callers.
    pub(crate) fn process(&self) -> ProcessId {
        self.process
    }

    /// Lends the child's pidfd, if [`with_pidfd`](Self::with_pidfd) opened one or
    /// [`from_pidfd`](Self::from_pidfd) took one, which polls readable once the child ends.
    pub fn pidfd(&self) -> Option<BorrowedFd<'_>> {
        self.pidfd.as_ref().map(AsFd::as_fd)
    }

    /// Returns the status with which the child ended, once a wait or
    /// [`observe`](Self::observe) collected it.
    pub fn status(&self) -> Option<ExitStatus> {
        self.status
    }

    /// Hands the child the status that the caller's own collector, such as its `waitpid(-1)`
    /// loop, collected for the process `id`, and returns whether it was this child's final
    /// status, which the child then keeps.
    ///
    /// Only a status with which a process ended counts: an exit, or a terminating signal. A stop
    /// or a continuation, which a collector that waits with `WUNTRACED` or `WCONTINUED` reports
    /// while the process lives, is not recorded, and this returns `false`. Only the first final
    /// status counts too: once the child has one, a later status for its ID belongs to another
    /// process that the ID was reused for, and this returns `false`.
    pub fn observe(&mut self, id: u32, status: ExitStatus) -> bool {
        let ended = status.code().is_some() || status.signal().is_some();
        if !ended || self.status.is_some() || id != self.id() {
            return false;
        }
        self.status = Some(status);
        true
    }

    /// Collects the child's status if it has ended, without blocking, and returns it, or `None`
    /// while the child runs. Once the child has a status, this returns it again.
    ///
    /// It collects no other child's status. A child that a pidfd names, and whose status another
    /// collector took without handing it over with [`observe`](Self::observe), is `ECHILD`.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.collect(false)
    }

    /// Waits until the child ends, collects its status, and returns it. Once the child has a
    /// status, this returns it again at once.
    ///
    /// It collects no other child's status. A child that a pidfd names, and whose status another
    /// collector took without handing it over with [`observe`](Self::observe), is `ECHILD`.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.collect(true)?
            .ok_or_else(|| io::Error::from(Errno::ECHILD))
    }

    /// Returns the child's status, which a wait for this child alone collects if the child has
    /// none yet, blocking until it ends when `block` is set.
    fn collect(&mut self, block: bool) -> io::Result<Option<ExitStatus>> {
        if self.status.is_none() {
            let target = match &self.pidfd {
                Some(pidfd) => WaitTarget::Pidfd(pidfd.as_fd()),
                None => WaitTarget::Process(self.process),
            };
            self.status = sys::wait_named(target, block)?.map(ExitStatus::from_raw);
        }
        Ok(self.status)
    }
}

/// Returns the process ID `id`, or `EINVAL` for one that no process can have.
fn process_id(id: u32) -> io::Result<ProcessId> {
    ProcessId::from_positive(i64::from(id)).ok_or_else(|| io::Error::from(Errno::EINVAL))
}

#[cfg(test)]
mod tests;
