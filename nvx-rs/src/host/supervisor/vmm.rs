//! The VMs that the supervisor holds, and the stops and terminations of their VMMs.

use std::io;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::host::launch::timeout_problem;
use crate::host::process::Process;
use crate::host::store::{LockedStore, RuntimeRecord, SandboxStore};

/// A VM that the supervisor holds: the runtime record of its launch, and its VMM, held through a
/// verified handle, as [`Starting::commit`] and [`reattach`] return it.
///
/// The VMM's exit event wakes its waiters, whether or not this caller started it.
///
/// [`Starting::commit`]: crate::host::supervisor::Starting::commit
/// [`reattach`]: fn@crate::host::supervisor::reattach
#[derive(Debug)]
pub struct Vmm {
    /// The sandbox's store.
    pub(super) store: SandboxStore,
    /// The runtime record of the VM's launch.
    pub(super) record: RuntimeRecord,
    /// The VMM, held through its verified handle.
    pub(super) process: Process,
}

impl Vmm {
    /// Returns the runtime record of the VM's launch: the reference to its VMM, its control
    /// endpoint, and the runtime identity that it was launched with.
    pub fn record(&self) -> &RuntimeRecord {
        &self.record
    }

    /// Returns the VM's control endpoint: a Unix socket path on Linux, and a named pipe on
    /// Windows.
    pub fn endpoint(&self) -> &str {
        self.record.endpoint()
    }

    /// Returns the VMM, held through its verified handle.
    pub fn process(&self) -> &Process {
        &self.process
    }

    /// Waits until the VMM exits or `deadline` passes, and returns whether it exited, as
    /// [`Process::wait`] does.
    pub fn wait(&self, deadline: Instant) -> io::Result<bool> {
        self.process.wait(deadline)
    }

    /// Returns whether the VMM has exited, without waiting.
    pub fn has_exited(&self) -> io::Result<bool> {
        self.process.has_exited()
    }

    /// Stops the VM: asks it to shut down through the caller's graceful step, waits for its VMM
    /// to exit until `deadline`, and takes the forced step if the caller requests it with
    /// `forced` and the VMM still runs.
    ///
    /// - The graceful step, `graceful`, runs once, with this VM and `deadline`, unless the VMM
    ///   has exited already. It asks the guest to shut down through the caller's own transport,
    ///   such as its control session, which the supervisor does not own. Once it succeeds, the
    ///   stop waits until the VMM exits or `deadline` passes. A graceful step that fails may not
    ///   have reached the guest, so the stop then only checks whether the VMM has exited, and
    ///   keeps the step's error in [`Stopped::graceful_error`].
    /// - The forced step terminates the VMM through its verified handle, and waits up to `forced`
    ///   for it to exit. Without `forced`, the stop never signals the VMM.
    ///
    /// The result says whether the VMM exited without the forced step, the forced step ended it,
    /// or it still runs; see [`StopOutcome`]. Once the VMM has exited, the stop clears the runtime
    /// record of its launch, as [`LockedStore::clear_runtime`] does, unless the record is another
    /// VM's by then. A VMM that still runs keeps its record. One that had exited before the stop
    /// began, which runs no step, ends as one that the stop ended, as [`StopOutcome::Graceful`],
    /// and its record is cleared: [`stop_unless_exited`](Self::stop_unless_exited) tells such a
    /// VMM apart, and keeps its record.
    ///
    /// `locked` must be the locked store of the VM's sandbox. Fails with
    /// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) if `forced` is zero or longer
    /// than [`MAX_TIMEOUT`], and fails when the VMM's state cannot be determined, or when the
    /// forced step cannot terminate it, as on Linux hosts without pidfds, which cannot hold it.
    ///
    /// [`MAX_TIMEOUT`]: crate::host::launch::MAX_TIMEOUT
    pub fn stop<E>(
        &self,
        locked: &mut LockedStore,
        deadline: Instant,
        forced: Option<Duration>,
        graceful: impl FnOnce(&Self, Instant) -> std::result::Result<(), E>,
    ) -> Result<Stopped<E>> {
        self.stop_with(locked, deadline, forced, graceful, &self.process)
    }

    /// Stops the VM as [`stop`](Self::stop) does, unless its VMM had exited before the stop began,
    /// and returns `None` then, with the runtime record of its launch kept.
    ///
    /// The stop decides by its own check of the VMM, before its graceful step, as
    /// [`stop`](Self::stop) does. A VMM that had exited by then ended without its holder, however
    /// recently the holder found it running: the stop runs no step, signals nothing, and clears
    /// nothing, so that the record tells [`reattach`] that the VM exited unheld, as one that exits
    /// while no caller holds it does. A caller that tells how the VM ended then reattaches, as
    /// [`reattach_clone`](crate::host::template::reattach_clone) does, which records a clone's
    /// failure, with the loss of its status if no report of the guest's own end tells why its VM
    /// exited, before it clears the record. [`stop`](Self::stop) ends such a VM as one that it
    /// stopped, and clears its record, so a holder that checks whether the VMM has exited, then
    /// stops the VM with it, attributes to its stop a VMM that exits in between.
    ///
    /// A VMM that runs as the stop begins is stopped as [`stop`](Self::stop) stops it, with the
    /// same result, and its exit is the stop's from then on. Fails as [`stop`](Self::stop) fails.
    ///
    /// [`reattach`]: fn@crate::host::supervisor::reattach
    pub fn stop_unless_exited<E>(
        &self,
        locked: &mut LockedStore,
        deadline: Instant,
        forced: Option<Duration>,
        graceful: impl FnOnce(&Self, Instant) -> std::result::Result<(), E>,
    ) -> Result<Option<Stopped<E>>> {
        self.stop_unless_exited_with(locked, deadline, forced, graceful, &self.process)
    }

    /// Ends the VM by identity-verified forced termination: terminates its VMM through its
    /// verified handle, and waits up to `bound` for it to exit. It is the forced step of
    /// [`stop`](Self::stop), taken alone, with no graceful step before it.
    ///
    /// It ends a VM whose guest the caller must not ask to shut down, such as one that another
    /// runtime identity started, which [`reattach`] reports as [`Reattached::Mismatched`]: its
    /// guest might not speak the caller's protocol, so the termination sends it nothing, and runs
    /// nothing of the caller's. It signals only the VMM that this holds, whose process ID, start
    /// time, and host boot were verified, never a process that reuses its ID.
    ///
    /// The result says whether the VMM exited: [`StopOutcome::Forced`] once the termination ended
    /// it within `bound`, [`StopOutcome::Graceful`] if it had exited before, and was not
    /// signaled, and [`StopOutcome::TimedOut`] if it still runs. Once the VMM has exited, the
    /// termination clears the runtime record of its launch, as [`stop`](Self::stop) does. A VMM
    /// that still runs, or whose exit cannot be determined, keeps its record, so that a later
    /// termination may retry.
    ///
    /// `locked` must be the locked store of the VM's sandbox. Fails with
    /// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) if `bound` is zero or longer
    /// than [`MAX_TIMEOUT`], and fails when the VMM's state cannot be determined, or when it cannot
    /// be terminated, as on Linux hosts without pidfds, which cannot hold it.
    ///
    /// [`reattach`]: fn@crate::host::supervisor::reattach
    /// [`Reattached::Mismatched`]: crate::host::supervisor::Reattached::Mismatched
    /// [`MAX_TIMEOUT`]: crate::host::launch::MAX_TIMEOUT
    pub fn terminate(&self, locked: &mut LockedStore, bound: Duration) -> Result<StopOutcome> {
        self.terminate_with(locked, bound, &self.process)
    }

    /// Stops the VM as [`stop`](Self::stop) does, through `process`: its VMM, or, in tests, a
    /// stand-in for it.
    pub(super) fn stop_with<E>(
        &self,
        locked: &mut LockedStore,
        deadline: Instant,
        forced: Option<Duration>,
        graceful: impl FnOnce(&Self, Instant) -> std::result::Result<(), E>,
        process: &impl Stoppable,
    ) -> Result<Stopped<E>> {
        if let Some(stopped) =
            self.stop_unless_exited_with(locked, deadline, forced, graceful, process)?
        {
            return Ok(stopped);
        }
        self.clear_if_exited(locked, StopOutcome::Graceful)?;
        Ok(Stopped {
            outcome: StopOutcome::Graceful,
            graceful_error: None,
        })
    }

    /// Stops the VM as [`stop_unless_exited`](Self::stop_unless_exited) does, through `process`:
    /// its VMM, or, in tests, a stand-in for it.
    pub(super) fn stop_unless_exited_with<E>(
        &self,
        locked: &mut LockedStore,
        deadline: Instant,
        forced: Option<Duration>,
        graceful: impl FnOnce(&Self, Instant) -> std::result::Result<(), E>,
        process: &impl Stoppable,
    ) -> Result<Option<Stopped<E>>> {
        self.check_store(locked)?;
        if let Some(bound) = forced {
            check_bound(bound, "the forced step")?;
        }
        let stopped = stop_process(process, self.record.process().pid, deadline, forced, || {
            graceful(self, deadline)
        })?;
        if let Some(stopped) = &stopped {
            self.clear_if_exited(locked, stopped.outcome)?;
        }
        Ok(stopped)
    }

    /// Ends the VM as [`terminate`](Self::terminate) does, through `process`: its VMM, or, in
    /// tests, a stand-in for it.
    pub(super) fn terminate_with(
        &self,
        locked: &mut LockedStore,
        bound: Duration,
        process: &impl Stoppable,
    ) -> Result<StopOutcome> {
        self.check_store(locked)?;
        check_bound(bound, "the termination")?;
        let outcome = terminate_process(process, self.record.process().pid, bound)?;
        self.clear_if_exited(locked, outcome)?;
        Ok(outcome)
    }

    /// Fails unless `locked` is the locked store of the VM's sandbox.
    fn check_store(&self, locked: &LockedStore) -> Result<()> {
        if locked.dir() == self.store.dir() {
            return Ok(());
        }
        Err(Error::other(format!(
            "the VM of the sandbox in {} cannot be stopped through the store in {}",
            self.store.dir().display(),
            locked.dir().display()
        )))
    }

    /// Clears the runtime record of the VM's launch from `locked` once its VMM has exited, which
    /// `outcome` tells, unless the record is another VM's by then: another launch's, or that of
    /// another VMM under a launch identity that its caller used again.
    fn clear_if_exited(&self, locked: &mut LockedStore, outcome: StopOutcome) -> Result<()> {
        if outcome != StopOutcome::TimedOut
            && locked
                .runtime()?
                .is_some_and(|record| record == self.record)
        {
            locked.clear_runtime_of(Some(&self.record))?;
        }
        Ok(())
    }
}

/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) if `bound`, the bound of
/// `what`, is zero or longer than [`MAX_TIMEOUT`].
///
/// [`MAX_TIMEOUT`]: crate::host::launch::MAX_TIMEOUT
fn check_bound(bound: Duration, what: &str) -> Result<()> {
    match timeout_problem(bound, true) {
        Some(problem) => Err(Error::unsupported(format!("the bound of {what} {problem}"))),
        None => Ok(()),
    }
}

/// How [`Vmm::stop`], [`Vmm::stop_unless_exited`], or [`Vmm::terminate`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StopOutcome {
    /// The VMM exited without the forced step: by the deadline of the graceful step, or before
    /// the stop or the termination began, which [`Vmm::stop_unless_exited`] reports as no stop.
    Graceful,
    /// The forced step terminated the VMM through its verified handle, and it exited within the
    /// step's bound: the graceful step of a stop did not end it, or a termination took the step
    /// alone.
    Forced,
    /// The VMM still runs: it did not exit by the deadline of the graceful step, or, after a
    /// graceful step that failed, had not exited, and the caller requested no forced step; or it
    /// did not exit within the bound of the forced step, which a termination takes alone.
    TimedOut,
}

/// What [`Vmm::stop`] or [`Vmm::stop_unless_exited`] did: how the stop ended, and the failure of
/// its graceful step, if it failed.
#[derive(Debug)]
pub struct Stopped<E> {
    /// How the stop ended.
    pub outcome: StopOutcome,
    /// The error of the graceful step, if the step ran and failed.
    pub graceful_error: Option<E>,
}

/// The operations on a VMM that a stop or a termination takes, which tests replace.
pub(super) trait Stoppable {
    /// Returns whether the VMM has exited, without waiting.
    fn has_exited(&self) -> io::Result<bool>;

    /// Waits until the VMM exits or `deadline` passes, and returns whether it exited.
    fn wait(&self, deadline: Instant) -> io::Result<bool>;

    /// Ends the VMM at once through its verified handle, without waiting.
    fn kill(&self) -> io::Result<()>;
}

impl Stoppable for Process {
    fn has_exited(&self) -> io::Result<bool> {
        Process::has_exited(self)
    }

    fn wait(&self, deadline: Instant) -> io::Result<bool> {
        Process::wait(self, deadline)
    }

    fn kill(&self) -> io::Result<()> {
        Process::kill(self)
    }
}

/// Stops the VMM `process`, whose ID is `pid`, with the graceful step `graceful`, the deadline
/// `deadline`, and the forced step that `forced` requests, as [`Vmm::stop_unless_exited`]
/// describes, without clearing its runtime record: returns `None`, and takes no step, if the VMM
/// had exited before.
pub(super) fn stop_process<E>(
    process: &impl Stoppable,
    pid: u32,
    deadline: Instant,
    forced: Option<Duration>,
    graceful: impl FnOnce() -> std::result::Result<(), E>,
) -> Result<Option<Stopped<E>>> {
    if process
        .has_exited()
        .map_err(|error| undetermined(pid, error))?
    {
        return Ok(None);
    }
    let graceful_error = graceful().err();
    let exited = match graceful_error {
        None => process.wait(deadline),
        Some(_) => process.has_exited(),
    }
    .map_err(|error| undetermined(pid, error))?;
    let outcome = match forced {
        _ if exited => StopOutcome::Graceful,
        None => StopOutcome::TimedOut,
        Some(bound) => force(process, pid, bound)?,
    };
    Ok(Some(Stopped {
        outcome,
        graceful_error,
    }))
}

/// Terminates the VMM `process`, whose ID is `pid`, unless it has exited, and waits up to `bound`
/// for it to exit, as [`Vmm::terminate`] describes, without clearing its runtime record.
pub(super) fn terminate_process(
    process: &impl Stoppable,
    pid: u32,
    bound: Duration,
) -> Result<StopOutcome> {
    if process
        .has_exited()
        .map_err(|error| undetermined(pid, error))?
    {
        return Ok(StopOutcome::Graceful);
    }
    force(process, pid, bound)
}

/// Takes the forced step of a stop or a termination: terminates the VMM `process`, whose ID is
/// `pid`, through its verified handle, and waits up to `bound` for it to exit.
fn force(process: &impl Stoppable, pid: u32, bound: Duration) -> Result<StopOutcome> {
    process.kill().map_err(|error| {
        Error::io(format!("cannot terminate VMM process {pid}")).with_source(error)
    })?;
    let exited = process
        .wait(Instant::now() + bound)
        .map_err(|error| undetermined(pid, error))?;
    Ok(if exited {
        StopOutcome::Forced
    } else {
        StopOutcome::TimedOut
    })
}

/// Returns the error of a stop or a termination that cannot determine whether VMM process `pid`
/// exited, for the reason `error`.
fn undetermined(pid: u32, error: io::Error) -> Error {
    Error::io(format!("cannot determine whether VMM process {pid} exited")).with_source(error)
}
