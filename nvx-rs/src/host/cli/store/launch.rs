//! Locked launch transitions, without spawning, recovery, or process termination.

use super::StateFile;
use super::files;
use super::locked::LockedState;
use super::record::{LaunchJournal, RuntimeRecord};
use crate::host::process::ProcessRef;
use crate::host::runtime::RuntimeIdentity;
use crate::host::vm::{Prepared, SavedVm};
use crate::{Error, Result};

/// A launch whose lock stays borrowed until commit or until its journal is left behind.
#[derive(Debug)]
pub(crate) struct Launch<'a> {
    locked: &'a mut LockedState,
    journal: LaunchJournal,
}

impl LockedState {
    /// Durably publishes the capability and complete prepared journal before the caller spawns.
    pub(crate) fn begin_launch(
        &mut self,
        prepared: &Prepared,
        runtime: &RuntimeIdentity,
    ) -> Result<Launch<'_>> {
        if self.journal()?.is_some() || self.runtime()?.is_some() {
            return Err(Error::other(
                "the sandbox already has a runtime record or an interrupted launch",
            ));
        }
        let journal = LaunchJournal::prepared(self, prepared, runtime)?;
        files::replace(self, StateFile::Capability, &journal.capability)?;
        self.write_record(StateFile::Journal, &journal)?;
        Ok(Launch {
            locked: self,
            journal,
        })
    }
}

impl Launch<'_> {
    /// Records only the caller's spawned process, durably, before changing the in-memory state.
    pub(crate) fn record_process(&mut self, process: &ProcessRef) -> Result<()> {
        if self.journal.process.is_some() {
            return Err(Error::other("the launch has recorded its process already"));
        }
        let journal = LaunchJournal {
            process: Some(process.clone()),
            ..self.journal.clone()
        };
        self.locked.write_record(StateFile::Journal, &journal)?;
        self.journal = journal;
        Ok(())
    }

    /// Publishes the exact saved VM, only after its matching process was journaled.
    ///
    /// The runtime record is the commit. A failed journal removal keeps that committed record
    /// and reports the failure; it never rolls back the commit or signals the VM.
    pub(crate) fn commit(self, saved: &SavedVm) -> Result<()> {
        if self.journal.process.as_ref() != Some(&saved.process)
            || self.journal.vm_dir != saved.vm_dir.path()
            || self.journal.endpoint != saved.endpoint
            || self.journal.capability != saved.capability
            || self.journal.runtime != saved.runtime
        {
            return Err(Error::other(
                "the saved VM does not match the launch and its journaled process",
            ));
        }
        self.locked
            .write_record(StateFile::Runtime, &RuntimeRecord::from(saved))?;
        files::remove(self.locked, StateFile::Journal).map_err(|error| {
            Error::io(format!(
                "the VM runtime was committed but its launch journal could not be removed: {error}"
            ))
            .with_source(error)
        })
    }
}
