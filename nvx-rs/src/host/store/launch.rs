//! A launch in progress, from the marker of its journal to its runtime record, and how a recovery
//! or an abandon ends one.

use std::fs::File;

use super::locked::LockedStore;
use super::record::{LaunchJournal, RuntimeRecord};
use super::{record, vmm};
use crate::error::{Error, Result};
use crate::host::process::{Process, ProcessRef};
use crate::host::spawn::Launched;

/// Ends a launch by terminating its verified VMM `process`.
pub(super) fn end_process(process: &Process) -> Recovery {
    let pid = process.reference().pid;
    match vmm::terminate(process) {
        Ok(true) => Recovery::Terminated(process.reference().clone()),
        Ok(false) => Recovery::Indeterminate(format!(
            "its VMM process {pid} did not exit after it was terminated"
        )),
        Err(error) => Recovery::Indeterminate(format!(
            "its VMM process {pid} cannot be terminated: {error}"
        )),
    }
}

/// How [`LockedStore::recover`] or [`Launch::abandon`] ended a launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovery {
    /// No launch was interrupted.
    Clean,
    /// The launch never spawned its VMM, or its VMM has exited; its files are removed.
    Exited,
    /// Recovery terminated the launch's VMM, whose reference this holds, after verifying it; its
    /// files are removed.
    Terminated(ProcessRef),
    /// Recovery cannot prove that nothing of the launch runs, for the reason that this gives, and
    /// keeps its files for a later recovery.
    Indeterminate(String),
}

/// A launch in progress, which borrows its sandbox's locked store exclusively from its journal's
/// marker until its runtime record.
///
/// Dropping it before [`commit`](Self::commit) or [`abandon`](Self::abandon) leaves its journal,
/// as a crash would, for [`LockedStore::recover`] to end.
#[derive(Debug)]
pub struct Launch<'a> {
    /// The sandbox's locked store.
    pub(super) locked: &'a mut LockedStore,
    /// The launch's journal, as written.
    pub(super) journal: LaunchJournal,
    /// The claimed log, until the caller takes it for the VMM.
    pub(super) log: Option<File>,
}

impl Launch<'_> {
    /// Returns the launch's journal.
    pub fn journal(&self) -> &LaunchJournal {
        &self.journal
    }

    /// Returns the control endpoint that the launch chose, on which the VMM must listen.
    pub fn endpoint(&self) -> &str {
        self.journal.endpoint()
    }

    /// Takes the claimed log, which the caller passes to the VMM as its standard output and
    /// error, as [`spawn`] does with its `log`, once.
    ///
    /// The launch keeps no copy of the log, so the claim lasts as long as the processes that hold
    /// it.
    ///
    /// [`spawn`]: crate::host::supervisor::spawn
    pub fn take_log(&mut self) -> Result<File> {
        self.log
            .take()
            .ok_or_else(|| Error::other("the log of the launch was taken already"))
    }

    /// Records the reference to the VMM process `child`, which the caller spawned and has not
    /// reaped, right after the spawn, and returns the process, held through a handle of its own
    /// as [`Launched::process`] holds it.
    ///
    /// Fails if the launch has recorded its VMM already, or if the process's identity cannot be
    /// read; [`abandon`](Self::abandon) then ends the launch.
    pub fn record_process(&mut self, child: &Launched) -> Result<Process> {
        if self.journal.process().is_some() {
            return Err(Error::other(
                "the launch has recorded its VMM process already",
            ));
        }
        let process = child
            .process()
            .map_err(|error| Error::io("cannot identify the VMM process").with_source(error))?;
        let journal = self.journal.spawned(process.reference().clone());
        record::write(&self.locked.journal_file(), &journal)?;
        self.journal = journal;
        Ok(process)
    }

    /// Commits the launch of a VM that is ready: writes its runtime record, which then names the
    /// VM, and removes the journal.
    ///
    /// The runtime record is the commit, so a journal that remains because its removal failed is
    /// removed by [`LockedStore::recover`]. Fails if the launch has not recorded its VMM process,
    /// or if the runtime record cannot be written; the journal then remains, as that of an
    /// interrupted launch, for [`LockedStore::recover`] to end.
    pub fn commit(self) -> Result<RuntimeRecord> {
        let Some(process) = self.journal.process() else {
            return Err(Error::other(
                "a launch cannot commit before it records its VMM process",
            ));
        };
        let runtime = RuntimeRecord::committed(&self.journal, process.clone());
        record::write(&self.locked.runtime_file(), &runtime)?;
        let _ = self.locked.journal_file().remove_if_present();
        Ok(runtime)
    }

    /// Ends a launch that failed, as [`LockedStore::recover`] ends an interrupted one: it
    /// terminates the VMM if its identity is verified, and removes the launch's files once
    /// nothing of it runs.
    ///
    /// A caller that still holds the VMM's [`Launched`] process releases it as usual. If the
    /// launch never recorded that process, the caller ends it with [`Launched::kill_and_wait`]
    /// first: an unrecorded VMM that serves no endpoint yet holds the log's claim, so the launch
    /// would end as [`Recovery::Indeterminate`].
    pub fn abandon(mut self) -> Result<Recovery> {
        // A log that no VMM received holds no claim once it closes.
        drop(self.log.take());
        self.locked.end_launch(&self.journal)
    }
}
