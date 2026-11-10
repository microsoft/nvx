//! Locked launch transitions and bounded recovery of an interrupted launch.

use std::thread;
use std::time::{Duration, Instant};

use super::StateFile;
use super::files;
use super::locked::LockedState;
use super::record::{LaunchJournal, RuntimeRecord};
use crate::host::cli::deadline::RETRY_INTERVAL;
use crate::host::process::{BootId, KILL_TIMEOUT, Opened, Process, ProcessRef};
use crate::host::runtime::RuntimeIdentity;
use crate::host::vm::{Prepared, SavedVm};
use crate::{Error, ErrorKind, Result};

/// A launch whose lock stays borrowed until commit or until its journal is left behind.
#[derive(Debug)]
pub(crate) struct Launch<'a> {
    locked: &'a mut LockedState,
    journal: LaunchJournal,
}

impl LockedState {
    /// Resolves only an uncommitted journal, before the next command can mutate state.
    pub(crate) fn recover(&mut self, deadline: Instant) -> Result<()> {
        if self.runtime()?.is_some() {
            return Ok(());
        }
        let Some(journal) = self.journal()? else {
            return Ok(());
        };
        if journal.vm_dir != self.dir() || journal.log != self.path(StateFile::Log) {
            return Err(Error::io(
                "the launch journal belongs to another sandbox directory",
            ));
        }
        match journal.process() {
            Some(reference) => self.end_recorded(&journal, reference, deadline)?,
            None if !self.released_claim(&journal, deadline)? => {
                let process = self.endpoint_owner(&journal)?;
                end_process(&process, deadline)?;
            }
            None => {}
        }
        if self.capability()?.is_some() {
            files::remove(self, StateFile::Capability)?;
        }
        // Keep the journal until cleanup has finished, including after a partial removal.
        files::remove(self, StateFile::Journal)
    }

    /// Ends only the recorded process, or accepts an independently proved release of its claim.
    fn end_recorded(
        &self,
        journal: &LaunchJournal,
        reference: &ProcessRef,
        deadline: Instant,
    ) -> Result<()> {
        let boot = BootId::current().map_err(|error| {
            Error::io("cannot identify the current host boot").with_source(error)
        })?;
        if boot != reference.boot {
            return Ok(());
        }
        let unheld = match Process::open(reference) {
            Ok(Opened::Running(process)) => return end_process(&process, deadline),
            Ok(Opened::Exited(_)) => Error::new(
                ErrorKind::StillRunning,
                "the recorded VM exited, but a process may still hold its launch log",
            ),
            Err(error) => Error::io("cannot verify the recorded VM process").with_source(error),
        };
        if self.released_claim(journal, deadline)? {
            Ok(())
        } else {
            Err(unheld)
        }
    }

    /// Rechecks transient fork-inherited descriptors for at most one second.
    fn released_claim(&self, journal: &LaunchJournal, deadline: Instant) -> Result<bool> {
        let until = deadline.min(Instant::now() + Duration::from_secs(1));
        loop {
            if !self.claim_held(journal)? {
                return Ok(true);
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(false);
            }
            thread::sleep(left.min(RETRY_INTERVAL));
        }
    }

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

/// Ends a held, verified process and requires confirmation of its exit before the deadline.
fn end_process(process: &Process, deadline: Instant) -> Result<()> {
    let failed = |message: &str| {
        Error::new(
            ErrorKind::StillRunning,
            format!(
                "cannot confirm exit of VM process {}: {message}",
                process.reference().pid
            ),
        )
    };
    if process
        .wait(Instant::now())
        .map_err(|error| failed("the exit check failed").with_source(error))?
        .is_some()
    {
        return Ok(());
    }
    let until = deadline.min(Instant::now() + KILL_TIMEOUT);
    if until <= Instant::now() {
        return Err(failed("the recovery deadline passed"));
    }
    let kill_error = process.kill().err();
    let exited = process.wait(until).map_err(|error| {
        let mut failure = failed("the exit wait failed");
        if let Some(kill_error) = &kill_error {
            failure = failed(&format!(
                "termination failed: {kill_error}; the exit wait failed"
            ));
        }
        failure.with_source(error)
    })?;
    if exited.is_some() {
        return Ok(());
    }
    let failure = failed("the verified process has not exited");
    Err(match kill_error {
        Some(error) => failure.with_source(error),
        None => failure,
    })
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
