//! The supervisor of a sandbox's VMM, beneath the CLI, [`instance`](super::instance), and
//! external orchestrators: it launches OpenVMM through the [`store`](super::store)'s launch
//! journal, passing it only the descriptors or handles that the caller allows, waits for its exit
//! by event, stops it, and reattaches to it after the orchestrator restarts within the same host
//! boot.
//!
//! Callers keep the configuration, the launch planning, the authorization, and the application
//! protocol. The supervisor owns no control session: the caller checks that a launch is ready, and
//! asks a guest to shut down, through its own transport.
//!
//! # Launches
//!
//! [`launch`] runs the store's launch steps under the sandbox's lock:
//!
//! 1. [`LockedStore::begin_launch`] writes the journal's marker, and the caller's `command` builds
//!    the VMM's [`VmmCommand`] with the control endpoint that the launch chose.
//! 2. [`spawn`] starts the VMM with the launch's claimed log as its standard output and error, and
//!    [`Launch::record_process`] records its identity right after.
//! 3. The caller checks, through its own transport, that the VMM serves its control endpoint, and
//!    anything else that it requires of a launch, then [`Starting::commit`]s it, which writes the
//!    runtime record, or [`Starting::abandon`]s it.
//!
//! A launch that fails at any step ends as [`Launch::abandon`] ends it, and never leaves an
//! unrecorded VMM running: a VMM whose identity is not recorded is ended through its handle first.
//!
//! ```no_run
//! use std::time::{Duration, Instant};
//!
//! use nvx::host::launch::LaunchSpec;
//! use nvx::host::store::{RuntimeIdentity, SandboxStore};
//! use nvx::host::supervisor::{self, Reattached, StopOutcome, Vmm, VmmCommand};
//! use nvx::protocol::CAPABILITY_LEN;
//!
//! /// Finds the sandbox's VM again after a restart of the orchestrator, or launches it.
//! fn run(
//!     store: &SandboxStore,
//!     spec: &LaunchSpec,
//!     runtime: &RuntimeIdentity,
//!     capability: &[u8; CAPABILITY_LEN],
//! ) -> nvx::Result<Vmm> {
//!     let mut locked = store.lock(Some(Instant::now() + Duration::from_secs(10)))?;
//!     match supervisor::reattach(&mut locked, runtime)? {
//!         Reattached::Live(vmm) => return Ok(vmm),
//!         Reattached::Exited { .. } => {}
//!         // Other artifacts or contracts started the VM: end it through its verified handle,
//!         // without a graceful step that it might not understand.
//!         Reattached::Mismatched { vmm, .. } => {
//!             if vmm.terminate(&mut locked, Duration::from_secs(10))? == StopOutcome::TimedOut {
//!                 return Err(nvx::Error::timed_out("the VM of another runtime still runs"));
//!             }
//!         }
//!         Reattached::Indeterminate(reason) => return Err(nvx::Error::other(reason)),
//!     }
//!     let starting = supervisor::launch(&mut locked, runtime, capability, |endpoint| {
//!         let arguments = spec.openvmm_arguments(endpoint, &store.outcome_path())?;
//!         Ok(VmmCommand::new(&spec.openvmm, arguments, store.dir()))
//!     })?;
//!     // Connect to `starting.endpoint()` through the caller's own transport, then commit once
//!     // the VMM serves it, or else abandon the launch.
//!     starting.commit()
//! }
//! ```
//!
//! # Inheritance
//!
//! The VMM receives its capability on its standard input, its claimed log as its standard output
//! and error, and the descriptors (Linux) or handles (Windows) that the caller names in
//! [`VmmCommand::inherit`], under the numbers that they have in the caller, and nothing else of
//! the caller's. On Linux, every other descriptor stays close-on-exec, and only the VMM's own
//! process clears the flag of the allowed ones, so a process that another thread spawns meanwhile
//! inherits none of them. On Windows, the allowed handles join the explicit list of handles that
//! the VMM inherits, and stay inheritable only while it is created; see [`Inherited`].
//!
//! # Exit waits and stops
//!
//! A [`Vmm`] holds its VMM through a [`Process`]: a pidfd on Linux and a process handle on
//! Windows, whose exit event wakes waiters, whether or not this caller started the VMM. Linux
//! hosts without pidfds poll the VMM's identity instead.
//!
//! [`Vmm::stop`] runs one graceful step of the caller's, with a bounded deadline, and takes a
//! bounded forced step against the VMM's verified identity only when the caller requests it.
//! [`Vmm::terminate`] takes that forced step alone, with no graceful step: it ends a VM whose
//! guest the caller cannot ask to shut down, such as one that another runtime identity started.
//!
//! # Reattach
//!
//! After an orchestrator restarts within the same host boot, [`reattach`] ends an interrupted
//! launch with [`LockedStore::recover`], compares the runtime identity of the recorded VM with the
//! caller's own, with [`RuntimeRecord::check_identity`], and holds the VMM with
//! [`Process::open`]. A record from an earlier boot names a VMM that has exited. A VM of another
//! runtime identity is ended with [`Vmm::terminate`], never through the caller's own protocol,
//! which its guest might not speak.
//!
//! The supervisor signals only a process that it holds through a verified identity: the child
//! that it has not released, a [`Process`] that matched its recorded reference, or, during the
//! recovery of a launch that did not record its VMM, the process that recovery verified to be that
//! VMM. It never signals a process that reuses a recorded ID.

#[cfg(test)]
mod tests;

use std::io;
use std::time::{Duration, Instant};

use super::launch::timeout_problem;
pub use super::platform::inherit::Inherited;
use super::process::{Process, ProcessRef};
pub use super::spawn::{Launched, VmmCommand, spawn};
use super::store::{
    IdentityCheck, IdentityPart, Launch, LaunchJournal, LockedStore, Recovery, RuntimeIdentity,
    RuntimeRecord, SandboxStore,
};
use crate::error::{Error, Result};
use crate::protocol::CAPABILITY_LEN;

/// Launches a VMM in the sandbox of `locked`, through the store's launch journal, under the
/// runtime identity `runtime`, with `capability` on its standard input.
///
/// It begins the launch with [`LockedStore::begin_launch`], asks `command` for the VMM's command,
/// given the control endpoint that the launch chose, [`spawn`]s the VMM with the launch's claimed
/// log, and records its identity right after with [`Launch::record_process`]. The caller then
/// checks that the VMM serves its endpoint, through its own transport, and commits or abandons the
/// returned [`Starting`].
///
/// A step that fails ends the launch as [`Launch::abandon`] ends it, after it has ended a VMM whose
/// identity it could not record through its handle, and returns the step's error. A launch that
/// cannot be ended then, because its VMM did not exit, keeps its journal for
/// [`LockedStore::recover`], and the error says so. Fails as [`LockedStore::begin_launch`] does if
/// the sandbox's VM runs, or an interrupted launch has not been recovered.
pub fn launch<'s, 'h>(
    locked: &'s mut LockedStore,
    runtime: &RuntimeIdentity,
    capability: &[u8; CAPABILITY_LEN],
    command: impl FnOnce(&str) -> Result<VmmCommand<'h>>,
) -> Result<Starting<'s>> {
    let store = SandboxStore::clone(locked);
    let mut launch = locked.begin_launch(runtime, capability)?;
    let command = match command(launch.endpoint()) {
        Ok(command) => command,
        Err(error) => return Err(abandon_after(launch, error)),
    };
    let log = match launch.take_log() {
        Ok(log) => log,
        Err(error) => return Err(abandon_after(launch, error)),
    };
    let child = match spawn(&command, capability, log) {
        Ok(child) => child,
        Err(error) => {
            let error = Error::io(format!("cannot launch {}", command.program.display()))
                .with_source(error);
            return Err(abandon_after(launch, error));
        }
    };
    let process = match launch.record_process(&child) {
        Ok(process) => process,
        Err(error) => {
            // An unrecorded VMM holds the claim of its launch, so only its handle can end it,
            // before the launch ends.
            child.kill_and_wait();
            return Err(abandon_after(launch, error));
        }
    };
    Ok(Starting {
        inner: Some(StartingInner {
            launch,
            child,
            process,
        }),
        store,
    })
}

/// Ends `launch`, which failed with `error`, and returns `error`, which says why the launch could
/// not be ended if it could not: its journal then remains for [`LockedStore::recover`].
fn abandon_after(launch: Launch<'_>, error: Error) -> Error {
    let reason = match launch.abandon() {
        Ok(Recovery::Indeterminate(reason)) => reason,
        Ok(_) => return error,
        Err(failure) => failure.to_string(),
    };
    let (kind, message, source) = error.into_parts();
    let error = Error::new(
        kind,
        format!("{message}; the launch could not be ended, and remains for recovery: {reason}"),
    );
    match source {
        Some(source) => error.with_source(source),
        None => error,
    }
}

/// A launch whose VMM runs, and whose journal records the VMM's identity, until the caller
/// [commits](Self::commit) it, once the VMM serves its control endpoint, or
/// [abandons](Self::abandon) it.
///
/// It borrows the sandbox's locked store exclusively, as the store's [`Launch`] does, so no other
/// lifecycle transition of the sandbox overlaps it. Dropping it without committing it abandons the
/// launch, as [`abandon`](Self::abandon) does, ignoring failures; a crash of the caller leaves the
/// launch's journal instead, for [`LockedStore::recover`].
#[derive(Debug)]
pub struct Starting<'s> {
    /// The launch and its VMM, until the launch is committed or abandoned.
    inner: Option<StartingInner<'s>>,
    /// The sandbox's store.
    store: SandboxStore,
}

/// The launch of a [`Starting`], and its VMM.
#[derive(Debug)]
struct StartingInner<'s> {
    /// The store's launch, whose journal records the VMM.
    launch: Launch<'s>,
    /// The VMM's child, which the launch has not released.
    child: Launched,
    /// The VMM, held through its verified handle.
    process: Process,
}

impl StartingInner<'_> {
    /// Ends the launch as [`Launch::abandon`] ends it, and releases its VMM's child.
    fn abandon(self) -> Result<Recovery> {
        let Self {
            launch,
            child,
            process: _,
        } = self;
        let recovery = launch.abandon();
        match &recovery {
            Ok(Recovery::Clean | Recovery::Exited | Recovery::Terminated(_)) => child.detach(),
            // The VMM may still run: its handle ends it, and the journal that remains then shows
            // that it exited.
            Ok(Recovery::Indeterminate(_)) | Err(_) => {
                child.kill_and_wait();
            }
        }
        recovery
    }
}

impl<'s> Starting<'s> {
    /// Returns the launch and its VMM.
    fn inner(&self) -> &StartingInner<'s> {
        self.inner
            .as_ref()
            .expect("a launch is in progress until it is committed or abandoned")
    }

    /// Returns the control endpoint that the launch chose, on which the VMM listens.
    pub fn endpoint(&self) -> &str {
        self.inner().launch.endpoint()
    }

    /// Returns the launch's journal, which records the VMM's identity.
    pub fn journal(&self) -> &LaunchJournal {
        self.inner().launch.journal()
    }

    /// Returns the VMM, held through its verified handle, through which the caller may wait for
    /// it, for example while it checks that the VMM serves its endpoint.
    pub fn process(&self) -> &Process {
        &self.inner().process
    }

    /// Returns the VMM's child, through which the CLI waits for a VMM whose start failed, reads
    /// its exit status, and ends it as `subprocess` ends it, before it abandons the launch.
    #[cfg(feature = "cli")]
    pub(crate) fn child(&mut self) -> &mut Launched {
        &mut self
            .inner
            .as_mut()
            .expect("a launch is in progress until it is committed or abandoned")
            .child
    }

    /// Commits the launch of a VMM that serves its control endpoint: writes its runtime record,
    /// as [`Launch::commit`] does, releases the VMM's child, which keeps running, and returns the
    /// VM.
    ///
    /// Fails if the runtime record cannot be written. The VMM is then ended through its handle,
    /// and the journal that remains shows [`LockedStore::recover`] that it exited.
    pub fn commit(mut self) -> Result<Vmm> {
        let StartingInner {
            launch,
            child,
            process,
        } = self.inner.take().expect("a launch commits once");
        match launch.commit() {
            Ok(record) => {
                child.detach();
                Ok(Vmm {
                    store: self.store.clone(),
                    record,
                    process,
                })
            }
            Err(error) => {
                // No runtime record names the VM, so its VMM must not run on.
                child.kill_and_wait();
                Err(error)
            }
        }
    }

    /// Ends a launch that failed, as [`Launch::abandon`] does: terminates its verified VMM, and
    /// removes the launch's files once nothing of it runs.
    ///
    /// A launch that it cannot end, which [`Recovery::Indeterminate`] reports, keeps its journal
    /// for [`LockedStore::recover`], after its VMM is ended through its handle.
    pub fn abandon(mut self) -> Result<Recovery> {
        self.inner
            .take()
            .expect("a launch is abandoned once")
            .abandon()
    }
}

impl Drop for Starting<'_> {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            let _ = inner.abandon();
        }
    }
}

/// A VM that the supervisor holds: the runtime record of its launch, and its VMM, held through a
/// verified handle, as [`Starting::commit`] and [`reattach`] return it.
///
/// The VMM's exit event wakes its waiters, whether or not this caller started it.
#[derive(Debug)]
pub struct Vmm {
    /// The sandbox's store.
    store: SandboxStore,
    /// The runtime record of the VM's launch.
    record: RuntimeRecord,
    /// The VMM, held through its verified handle.
    process: Process,
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
    /// record of its launch, as [`LockedStore::clear_runtime`] does, unless the record names
    /// another launch by then. A VMM that still runs keeps its record.
    ///
    /// `locked` must be the locked store of the VM's sandbox. Fails with
    /// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) if `forced` is zero or longer
    /// than [`MAX_TIMEOUT`](super::launch::MAX_TIMEOUT), and fails when the VMM's state cannot be
    /// determined, or when the forced step cannot terminate it, as on Linux hosts without pidfds,
    /// which cannot hold it.
    pub fn stop<E>(
        &self,
        locked: &mut LockedStore,
        deadline: Instant,
        forced: Option<Duration>,
        graceful: impl FnOnce(&Self, Instant) -> std::result::Result<(), E>,
    ) -> Result<Stopped<E>> {
        self.check_store(locked)?;
        if let Some(bound) = forced {
            check_bound(bound, "the forced step")?;
        }
        let stopped = stop_process(
            &self.process,
            self.record.process().pid,
            deadline,
            forced,
            || graceful(self, deadline),
        )?;
        self.clear_if_exited(locked, stopped.outcome)?;
        Ok(stopped)
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
    /// than [`MAX_TIMEOUT`](super::launch::MAX_TIMEOUT), and fails when the VMM's state cannot be
    /// determined, or when it cannot be terminated, as on Linux hosts without pidfds, which cannot
    /// hold it.
    pub fn terminate(&self, locked: &mut LockedStore, bound: Duration) -> Result<StopOutcome> {
        self.check_store(locked)?;
        check_bound(bound, "the termination")?;
        let outcome = terminate_process(&self.process, self.record.process().pid, bound)?;
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
    /// `outcome` tells, unless the record names another launch by then.
    fn clear_if_exited(&self, locked: &mut LockedStore, outcome: StopOutcome) -> Result<()> {
        if outcome != StopOutcome::TimedOut
            && locked
                .runtime()?
                .is_some_and(|record| record.launch() == self.record.launch())
        {
            locked.clear_runtime()?;
        }
        Ok(())
    }
}

/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) if `bound`, the bound of
/// `what`, is zero or longer than [`MAX_TIMEOUT`](super::launch::MAX_TIMEOUT).
fn check_bound(bound: Duration, what: &str) -> Result<()> {
    match timeout_problem(bound, true) {
        Some(problem) => Err(Error::unsupported(format!("the bound of {what} {problem}"))),
        None => Ok(()),
    }
}

/// How [`Vmm::stop`] or [`Vmm::terminate`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StopOutcome {
    /// The VMM exited without the forced step: by the deadline of the graceful step, or before
    /// the stop or the termination began.
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

/// What [`Vmm::stop`] did: how the stop ended, and the failure of its graceful step, if it
/// failed.
#[derive(Debug)]
pub struct Stopped<E> {
    /// How the stop ended.
    pub outcome: StopOutcome,
    /// The error of the graceful step, if the step ran and failed.
    pub graceful_error: Option<E>,
}

/// The operations on a VMM that a stop or a termination takes, which tests replace.
trait Stoppable {
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
/// `deadline`, and the forced step that `forced` requests, as [`Vmm::stop`] describes, without
/// clearing its runtime record.
fn stop_process<E>(
    process: &impl Stoppable,
    pid: u32,
    deadline: Instant,
    forced: Option<Duration>,
    graceful: impl FnOnce() -> std::result::Result<(), E>,
) -> Result<Stopped<E>> {
    if process
        .has_exited()
        .map_err(|error| undetermined(pid, error))?
    {
        return Ok(Stopped {
            outcome: StopOutcome::Graceful,
            graceful_error: None,
        });
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
    Ok(Stopped {
        outcome,
        graceful_error,
    })
}

/// Terminates the VMM `process`, whose ID is `pid`, unless it has exited, and waits up to `bound`
/// for it to exit, as [`Vmm::terminate`] describes, without clearing its runtime record.
fn terminate_process(process: &impl Stoppable, pid: u32, bound: Duration) -> Result<StopOutcome> {
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

/// What [`reattach`] found of the VM of a sandbox.
#[derive(Debug)]
pub enum Reattached {
    /// The recorded VMM runs, and was launched with the caller's runtime identity: the caller may
    /// keep driving the VM, which it now holds.
    Live(Vmm),
    /// The recorded VMM runs, but was launched with another runtime identity, which would drive
    /// the VM through artifacts or contracts that did not start it: the caller must end it with
    /// [`Vmm::terminate`], which signals only the VMM that the [`Vmm`] holds by its verified
    /// identity, and asks its guest nothing.
    Mismatched {
        /// The VM, which the caller must end.
        vmm: Vmm,
        /// The parts of the runtime identity that differ, as
        /// [`RuntimeIdentity::differences`] returns them.
        differences: Vec<IdentityPart>,
    },
    /// No VMM of the sandbox runs: none was recorded, or the recorded one exited, as one that a
    /// record of an earlier boot names always has. The runtime record of a VMM that exited is
    /// cleared.
    Exited {
        /// The runtime record of the VMM that exited, which reattach cleared, if one was recorded.
        record: Option<RuntimeRecord>,
        /// How [`LockedStore::recover`] ended an interrupted launch: [`Recovery::Clean`] if none
        /// was interrupted.
        recovery: Recovery,
    },
    /// Whether a VMM of the sandbox runs cannot be determined, for the reason that this gives:
    /// an interrupted launch cannot be ended yet, or the recorded VMM cannot be examined. The
    /// runtime record, and the journal of an interrupted launch, remain for a later reattach,
    /// which may succeed.
    ///
    /// Reattach signals no process that it has not verified, but it is not free of effects:
    /// [`LockedStore::recover`] may have terminated a verified VMM of the interrupted launch that
    /// has not exited yet, and removes a journal that the runtime record of its committed launch
    /// supersedes.
    Indeterminate(String),
}

/// Reattaches to the VM of the sandbox of `locked` after the orchestrator restarts within the
/// same host boot, under the runtime identity `runtime`, which the caller would launch now.
///
/// It ends an interrupted launch with [`LockedStore::recover`], which signals only a VMM that it
/// verified, reads the runtime record, holds the recorded VMM with [`Process::open`], which
/// verifies its process ID, its start time, and the host boot, and compares the record's runtime
/// identity with `runtime`, as [`RuntimeRecord::check_identity`] does; see [`Reattached`]. A
/// record from an earlier boot names a VMM that has exited. Beyond that recovery, reattach
/// signals nothing: a VM that it reports as live or mismatched keeps running.
///
/// Reattach does not replay the VM's start or its mappings, restore streams that the restart
/// lost, or resolve images: the caller's own protocol resumes through the recorded endpoint, if it
/// can. Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for an invalid
/// `runtime`, and as [`LockedStore::recover`] does for inconsistent records.
pub fn reattach(locked: &mut LockedStore, runtime: &RuntimeIdentity) -> Result<Reattached> {
    reattach_with(locked, runtime, Process::open)
}

/// Reattaches as [`reattach`] does, holding the recorded VMM with `open`, which is
/// [`Process::open`] outside tests.
fn reattach_with(
    locked: &mut LockedStore,
    runtime: &RuntimeIdentity,
    open: impl FnOnce(&ProcessRef) -> io::Result<Option<Process>>,
) -> Result<Reattached> {
    runtime.check()?;
    let recovery = locked.recover()?;
    if let Recovery::Indeterminate(reason) = recovery {
        return Ok(Reattached::Indeterminate(format!(
            "an interrupted launch of the sandbox cannot be ended yet: {reason}"
        )));
    }
    let Some(record) = locked.runtime()? else {
        return Ok(Reattached::Exited {
            record: None,
            recovery,
        });
    };
    let pid = record.process().pid;
    let process = match open(record.process()) {
        Ok(Some(process)) => process,
        Ok(None) => {
            locked.clear_runtime()?;
            return Ok(Reattached::Exited {
                record: Some(record),
                recovery,
            });
        }
        Err(error) => {
            return Ok(Reattached::Indeterminate(format!(
                "the recorded VMM process {pid} cannot be examined: {error}"
            )));
        }
    };
    let check = record.check_identity(runtime);
    let vmm = Vmm {
        store: SandboxStore::clone(locked),
        record,
        process,
    };
    Ok(match check {
        IdentityCheck::Matches => Reattached::Live(vmm),
        IdentityCheck::Mismatched(differences) => Reattached::Mismatched { vmm, differences },
    })
}
