//! The launches of a sandbox's VMM through the store's launch journal, and the starting launches
//! that their callers commit or abandon.

use super::vmm::Vmm;
use crate::error::{Error, Result};
use crate::host::process::Process;
use crate::host::spawn::{Launched, VmmCommand, spawn};
use crate::host::store::{
    Launch, LaunchId, LaunchJournal, LockedStore, Recovery, RuntimeIdentity, SandboxStore,
};
use crate::protocol::CAPABILITY_LEN;

/// Launches a VMM in the sandbox of `locked`, through the store's launch journal, as the launch
/// `reserved`, which the caller reserved with [`LaunchId::generate`], or which the sandbox's lease
/// reserves, [`Lease::launch`](crate::host::template::Lease::launch), under the runtime identity
/// `runtime`, with `capability` on its standard input.
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
/// the sandbox's VM runs, an interrupted launch has not been recovered, or the sandbox holds a
/// lease that does not let this lock begin `reserved`: one that reserves another launch, or that
/// another lock acquired, or whose launch has begun.
pub fn launch<'s, 'h>(
    locked: &'s mut LockedStore,
    reserved: &LaunchId,
    runtime: &RuntimeIdentity,
    capability: &[u8; CAPABILITY_LEN],
    command: impl FnOnce(&str) -> Result<VmmCommand<'h>>,
) -> Result<Starting<'s>> {
    launch_reporting(
        locked,
        reserved,
        runtime,
        capability,
        command,
        |_, _| Ok(()),
    )
    .map_err(|failure| failure.error)
}

/// How a launch of [`launch_reporting`] failed.
#[derive(Debug)]
pub(crate) struct LaunchFailure {
    /// The error of the step that failed, which says why the launch could not be ended if it
    /// could not.
    pub(crate) error: Error,
    /// Whether the launch spawned its VMM, which it then ended through the VMM's handle.
    pub(crate) spawned: bool,
    /// Why the launch was not ended, whose journal then remains for [`LockedStore::recover`]:
    /// it could not be, or its caller could not record its failure first. `None` once it ended.
    pub(crate) unended: Option<String>,
}

/// Launches a VMM as [`launch`] does, and tells how a launch that fails ended, as a
/// [`LaunchFailure`]. A launch that began, and fails, calls `failing` with the step's error, and
/// whether it spawned the VMM, before it ends the launch, so that the caller may record the
/// failure, as a clone's launch does, before anything of the launch ends. If `failing` fails, the
/// launch ends a VMM that it spawned through the VMM's handle, as always, but keeps its journal,
/// as a crash would leave it, for [`LockedStore::recover`]: the journal then still tells that the
/// launch began, though the caller could not record its failure.
pub(crate) fn launch_reporting<'s, 'h>(
    locked: &'s mut LockedStore,
    reserved: &LaunchId,
    runtime: &RuntimeIdentity,
    capability: &[u8; CAPABILITY_LEN],
    command: impl FnOnce(&str) -> Result<VmmCommand<'h>>,
    failing: impl FnOnce(&Error, bool) -> Result<()>,
) -> std::result::Result<Starting<'s>, LaunchFailure> {
    let store = SandboxStore::clone(locked);
    // A launch that does not begin leaves nothing to end.
    let mut launch = locked
        .begin_launch(reserved, runtime, capability)
        .map_err(|error| LaunchFailure {
            error,
            spawned: false,
            unended: None,
        })?;
    let command = match command(launch.endpoint()) {
        Ok(command) => command,
        Err(error) => {
            let recorded = failing(&error, false);
            return Err(abandon_after(launch, error, false, recorded));
        }
    };
    let log = match launch.take_log() {
        Ok(log) => log,
        Err(error) => {
            let recorded = failing(&error, false);
            return Err(abandon_after(launch, error, false, recorded));
        }
    };
    let child = match spawn(&command, capability, log) {
        Ok(child) => child,
        Err(error) => {
            let error = Error::io(format!("cannot launch {}", command.program.display()))
                .with_source(error);
            let recorded = failing(&error, false);
            return Err(abandon_after(launch, error, false, recorded));
        }
    };
    let process = match launch.record_process(&child) {
        Ok(process) => process,
        Err(error) => {
            let recorded = failing(&error, true);
            // An unrecorded VMM holds the claim of its launch, so only its handle can end it,
            // before the launch ends.
            child.kill_and_wait();
            return Err(abandon_after(launch, error, true, recorded));
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

/// Ends `launch`, which failed with `error`, after it `spawned` a VMM or not, once its caller
/// `recorded` the failure, and returns how it failed, whose error says why the launch was not
/// ended if it was not: its journal then remains for [`LockedStore::recover`]. A launch whose
/// caller could not record its failure keeps its journal, as a crash would leave it, and ends no
/// more of it than its caller did.
fn abandon_after(
    launch: Launch<'_>,
    error: Error,
    spawned: bool,
    recorded: Result<()>,
) -> LaunchFailure {
    let reason = match recorded {
        Err(unrecorded) => {
            drop(launch);
            unrecorded.to_string()
        }
        Ok(()) => match launch.abandon() {
            Ok(Recovery::Indeterminate(reason)) => reason,
            Ok(_) => {
                return LaunchFailure {
                    error,
                    spawned,
                    unended: None,
                };
            }
            Err(failure) => failure.to_string(),
        },
    };
    let (kind, message, source) = error.into_parts();
    let error = Error::new(
        kind,
        format!("{message}; the launch could not be ended, and remains for recovery: {reason}"),
    );
    LaunchFailure {
        error: match source {
            Some(source) => error.with_source(source),
            None => error,
        },
        spawned,
        unended: Some(reason),
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
    pub fn commit(self) -> Result<Vmm> {
        self.commit_reporting(|_| {})
    }

    /// Commits the launch as [`commit`](Self::commit) does, and calls `failing` with the error of
    /// a commit that fails, before it ends the VMM, so that the caller may record the failure
    /// first, as a clone's launch does. The journal remains all the same, whether or not the
    /// caller could.
    pub(crate) fn commit_reporting(mut self, failing: impl FnOnce(&Error)) -> Result<Vmm> {
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
                failing(&error);
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

    /// Ends the launch's VMM through its handle, and keeps the launch's journal, as a crash of the
    /// caller would leave it, for [`LockedStore::recover`], which ends the launch once it finds
    /// the VMM exited. Returns whether the VMM exited, as [`Launched::kill_and_wait`] does.
    ///
    /// It ends a launch whose caller could not record its failure before the launch's end, as a
    /// clone's launch records it, so that the journal, which keeps the clone's lease, still tells
    /// that the launch began.
    pub(crate) fn end_keeping_journal(mut self) -> bool {
        let StartingInner {
            launch,
            child,
            process: _,
        } = self.inner.take().expect("a launch is ended once");
        let exited = child.kill_and_wait();
        // A launch that is dropped keeps its journal.
        drop(launch);
        exited
    }
}

impl Drop for Starting<'_> {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            let _ = inner.abandon();
        }
    }
}
