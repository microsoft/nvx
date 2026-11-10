//! Bounded launch proofs, without recovery, record removal, or process termination.

use std::io;

use super::StateFile;
use super::locked::LockedState;
use super::record::LaunchJournal;
use crate::host::platform;
use crate::host::process::{BootId, Process, ProcessRef};
use crate::{Error, Result};

impl LockedState {
    /// Returns whether a launch process may still run. Only a released claim proves exit;
    /// failure to examine it proves neither, and preserves the original error.
    pub(crate) fn claim_held(&self, journal: &LaunchJournal) -> Result<bool> {
        let path = self.path(StateFile::Log);
        if journal.vm_dir != self.dir() || journal.log != path {
            return Err(Error::io(
                "the launch journal belongs to another sandbox directory",
            ));
        }
        platform::log_claim_held(&path, journal.log_identity)
            .map_err(|error| Error::io("cannot check the launch log claim").with_source(error))
    }

    /// Holds the server of the recorded endpoint, rechecks its ownership, and on Linux proves
    /// that this very process also holds the launch's log. An error is not proof of exit.
    pub(crate) fn endpoint_owner(&self, journal: &LaunchJournal) -> Result<Process> {
        if !self.claim_held(journal)? {
            return Err(Error::io("the launch log claim is released"));
        }
        let process =
            hold_endpoint_server(|| platform::endpoint_server_pid(&journal.endpoint), find)?;
        let reference = process.reference();
        let matches =
            platform::serves_launch(reference.pid, reference.start_time, journal.log_identity)
                .map_err(|error| {
                    Error::io("cannot verify the endpoint owner's log").with_source(error)
                })?;
        if !matches {
            return Err(Error::io(
                "the endpoint owner does not hold the launch log as its standard output",
            ));
        }
        Ok(process)
    }
}

/// Opens only the process whose start time was just read, never another that reused its ID.
fn find(pid: u32) -> io::Result<Option<Process>> {
    let Some(start_time) = platform::process_start_time(pid)? else {
        return Ok(None);
    };
    Process::open(&ProcessRef {
        pid,
        start_time,
        boot: BootId::current()?,
    })
}

/// Adapted from the old store's owner check: the second endpoint lookup happens only after
/// the process is held. The callbacks keep replacement races testable without test-only hooks.
pub(super) fn hold_endpoint_server(
    mut server_pid: impl FnMut() -> io::Result<Option<u32>>,
    hold: impl FnOnce(u32) -> io::Result<Option<Process>>,
) -> Result<Process> {
    let pid = server_pid()
        .map_err(|error| Error::io("cannot examine the control endpoint").with_source(error))?
        .ok_or_else(|| Error::io("no process serves the control endpoint"))?;
    let process = hold(pid)
        .map_err(|error| Error::io("cannot hold the control endpoint owner").with_source(error))?
        .ok_or_else(|| Error::io("the control endpoint owner exited before it could be held"))?;
    let again = server_pid()
        .map_err(|error| Error::io("cannot recheck the control endpoint").with_source(error))?;
    if again != Some(pid) {
        return Err(Error::io(
            "the control endpoint changed while its owner was held",
        ));
    }
    Ok(process)
}
