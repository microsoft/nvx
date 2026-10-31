//! The reattaches to the VM of a sandbox after its orchestrator restarts within the same host
//! boot.

use std::io;

use super::vmm::Vmm;
use crate::error::Result;
use crate::host::process::{Process, ProcessRef};
use crate::host::store::{
    IdentityCheck, IdentityPart, LockedStore, Recovery, RuntimeIdentity, RuntimeRecord,
    SandboxStore,
};

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
    reattach_with(locked, runtime, Process::open, |_| Ok(()))
}

/// Reattaches as [`reattach`] does, and calls `exited` with the runtime record of a VMM that it
/// found exited, before it clears the record, so that the caller may record what ended the VM
/// while the sandbox still names it, as a clone's reattach does. Fails, and keeps the record, if
/// `exited` fails.
pub(crate) fn reattach_reporting(
    locked: &mut LockedStore,
    runtime: &RuntimeIdentity,
    exited: impl FnOnce(&RuntimeRecord) -> Result<()>,
) -> Result<Reattached> {
    reattach_with(locked, runtime, Process::open, exited)
}

/// Reattaches as [`reattach_reporting`] does, holding the recorded VMM with `open`, which is
/// [`Process::open`] outside tests.
pub(super) fn reattach_with(
    locked: &mut LockedStore,
    runtime: &RuntimeIdentity,
    open: impl FnOnce(&ProcessRef) -> io::Result<Option<Process>>,
    exited: impl FnOnce(&RuntimeRecord) -> Result<()>,
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
            exited(&record)?;
            locked.clear_runtime_of(Some(&record))?;
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
