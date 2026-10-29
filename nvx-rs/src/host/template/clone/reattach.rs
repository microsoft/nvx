//! The reattach of a clone, [`reattach_clone`], once the caller of its launch restarted: it finds
//! the clone's VM by the identity of its VMM's process, takes it over if this host may manage it,
//! and checks the clone's activation in a new session, or tells how the clone ended, from the
//! records of its sandbox.

use std::path::Path;
use std::time::{Duration, Instant};

use super::failure::{
    Attribute, CloneFailure, Evidence, Fault, Progress, Stage, Teardown, amended, describe,
};
use super::records::{Recorder, progress_of, read_failure_of, read_progress};
use super::restore::{ActivationSession, CloneControl};
use crate::activation::{ActivationRecord, MAX_RECORD_BYTES, ReadyActivation};
use crate::error::{Error, ErrorCode, Result};
use crate::host::launch::timeout_problem;
use crate::host::process::BootId;
use crate::host::report::Report;
use crate::host::store::files::sync_dir;
use crate::host::store::regular::{open_regular, read_at_most};
use crate::host::store::{IdentityPart, LockedStore, Recovery, RuntimeIdentity, RuntimeRecord};
use crate::host::supervisor::{self, Reattached, StopOutcome, Vmm};
use crate::host::template::Lease;

/// What [`reattach_clone`] found of a clone.
#[derive(Debug)]
#[non_exhaustive]
pub enum ReattachedClone<S> {
    /// The clone's VMM runs under the caller's runtime identity, and the clone answered the
    /// Activate of its record, byte for byte, in a new session of the adapter's framing, with the
    /// Ready of the record's activation: the caller holds the VM, and the session, in which it
    /// drives the clone.
    Live {
        /// The clone's VM.
        vmm: Box<Vmm>,
        /// The session in which the clone answered.
        session: S,
    },
    /// No VM of the clone runs, and the sandbox records no failure of it: its VM exited once its
    /// launch committed, with an outcome report of the guest's own end whose status is no evidence
    /// of a failure, or its holder stopped it, or its launch ended, or never began, without a
    /// failure, as its progress tells. The caller releases the clone's lease.
    Ended {
        /// How far the clone got.
        progress: Progress,
        /// OpenVMM's outcome report of the clone's VM, if its VMM wrote one.
        report: Option<Report>,
    },
    /// The clone failed, as the record of its failure in its sandbox tells, which the reattach
    /// found, or wrote: the launch of a caller that crashed, which the reattach ended, or which a
    /// recovery of the sandbox ended before, whose VM ended with a status of the guest's that is
    /// evidence; a VM that exited once its launch committed, with a status of the guest's that is
    /// evidence, or without a report of the guest's own end; a check of the clone that failed; a
    /// VM that runs under another runtime identity; or a record of a failure of the clone that
    /// cannot be read, which the record that the reattach wrote replaces. The failure's teardown
    /// tells whether a VM of the clone may still run: until it is resolved, the sandbox keeps the
    /// store's records of the VM, and with them the clone's lease, for a later reattach.
    Failed(CloneFailure),
    /// Whether the clone's VMM, whose launch committed, runs cannot be determined, for the reason
    /// that this gives, and the sandbox records no failure of the clone: the runtime record
    /// remains, and with it the clone's lease, for a later reattach, which may succeed.
    Indeterminate(String),
}

/// Reattaches to the clone of the sandbox of `locked`, under the runtime identity `runtime`,
/// which the caller would launch the clone's template with now, through the adapter's framing,
/// `control`, which must answer in a new session by `deadline`, and ends a VMM of the clone whose
/// launch committed, and that it may not keep, within `bound`.
///
/// It reads the records of the clone that the sandbox keeps, as [`CloneRecords`] describes, then
/// reattaches to the sandbox's VM, as [`supervisor::reattach`] does, which ends a launch that a
/// crash of its caller interrupted, through the verified identity of its VMM, as
/// [`LockedStore::recover`] ends it, within the wait that the store's recovery allows the VMM's
/// termination, which `bound` does not shorten, removes a journal that the runtime record of its
/// launch supersedes, as the launch's commit may leave it, which is no failure, and finds a VMM
/// whose launch committed by the identity of its process, which says which VM it is:
///
/// - A VMM that runs under `runtime`, whose launch is the one that the clone's lease reserves, is
///   the clone's, which the reattach checks in a new session, as the restore sequence of the
///   clone's launch does, [`Restoring::activate`](super::Restoring::activate), sending the
///   sandbox's activation record, byte for byte, which must still be the canonical encoding of a
///   record of the restore of the lease's generation: the clone answers with the Ready that it
///   answered once, and the reattach checks the transaction ID that it reports, which
///   [`ReattachedClone::Live`] returns with the session. The clone's launch committed once the
///   clone applied its mapping table, if it has one, and started its sandbox, so the reattach
///   sends neither again, and no launch that it ends gets either. A check that fails, by the
///   evidence that the restore sequence gives it, ends the VMM through its verified identity, as
///   [`Vmm::terminate`] does within `bound`, and the clone fails at [`Stage::Reattach`].
/// - A VMM that runs under another runtime identity, which might not speak the caller's
///   protocol, is ended the same way, without a word to its guest, and the clone fails at
///   [`Stage::Reattach`] with [`Evidence::RuntimeMismatch`], and
///   [`ErrorCode::RuntimeMismatch`].
/// - A launch that the reattach ended fails at the stage of its progress, with
///   [`ErrorCode::Exited`], and with [`Evidence::ForcedKill`] for a VMM that the reattach
///   terminated, and [`Evidence::LostStatus`] otherwise, which the error that OpenVMM reported, for
///   a VMM that exited before its restore-ready event, attributes as the restore sequence does. One
///   that it cannot end yet fails the same way with [`Evidence::LostStatus`], whose teardown is
///   unresolved: its journal remains, and the lease with it, for a later reattach.
/// - A launch that a recovery of the sandbox ended before the reattach, as [`supervisor::reattach`]
///   ends one, and of which the store holds no record, fails at the stage of its progress, with
///   [`ErrorCode::Exited`], if OpenVMM's outcome report gives a status of the guest's that is
///   evidence, and is [`ReattachedClone::Ended`] otherwise. Once the launch began, which removes
///   the report of an earlier VM of the sandbox before it creates the clone's progress log, a
///   report that the sandbox holds is the clone's VM's, even if no record kept the spawn of its
///   VMM, as a crash of the launch's caller before the journal or the progress log recorded it
///   leaves it.
/// - A VMM whose launch committed, and that exited, fails at [`Stage::Run`] with
///   [`ErrorCode::Exited`] if OpenVMM's outcome report gives a status of the guest's that is
///   evidence, as a status of the clone's repair is at any point, and with
///   [`Evidence::LostStatus`] if no report of the guest's own end tells why it exited, and
///   [`ReattachedClone::Ended`] otherwise. One that cannot be examined is
///   [`ReattachedClone::Indeterminate`]. A VM that its holder's stop ended, which cleared its
///   runtime record, lost no status, but one whose VMM had exited before the stop began is one
///   that exited, as above, since [`Vmm::stop_unless_exited`] keeps its record.
///
/// The evidence of a status of the guest's prevails over any but the template's, as in the restore
/// sequence, and as [the attribution of evidence](Evidence#attribution) describes. A failure is
/// recorded in the sandbox, as the restore sequence records it: before the reattach ends a VMM of
/// the clone, recovers an interrupted launch, or clears the runtime record of a VM that exited,
/// with its teardown [pending](Teardown::Pending), and again once the teardown ended. A reattach
/// that cannot record it then fails, before it ends or forgets anything of the clone's VM, as does
/// one that cannot make the record of an earlier failure durable. A clone
/// whose sandbox records a failure already failed for good: the reattach ends a VMM of the clone
/// that still runs, through its verified identity, then finishes a failure whose teardown a crash
/// of its caller left pending, as the failure would have recorded it once its teardown ended, or
/// records the teardown of one whose teardown was unresolved in place of the record's, which
/// keeps the stage, the evidence, and the original outcome, unless what the clone's VMM left as
/// it ended since tells more, as the guest's status, which the record does not hold, and the
/// error that OpenVMM reported, for a VMM that exited before its restore-ready event, may, and
/// returns the failure, with the record's lease, transaction, stage, progress, and code. A record
/// of a failure that cannot be read tells as much, though not how the clone failed, and so does
/// a record of another clone's failure, whose template store, lease, leased generation, its key,
/// or launch is not the binding's, and which tells nothing of this clone: if the store holds
/// records of the clone's launch, which the reattach may remove, the reattach first replaces it
/// with the record of its own failure at [`Stage::Reattach`], with the error of the record's read
/// and the host's evidence, its teardown pending, which keeps how far the clone got for a
/// reattach after a crash of the caller. It then ends a VMM of the clone that still runs the same
/// way, and records the failure with the teardown that it found or made, as any failure that it
/// records, over whose evidence a status of the guest's prevails. The failure that it returns is
/// then this clone's, so a [quarantine](CloneFailure::quarantine) of it never reaches a
/// generation that the clone does not lease.
///
/// The reattach reads no record of the template store: the lease holds the clone's generation,
/// and the sandbox's activation record the clone's transaction, so a tombstone, or a retirement,
/// of the generation leaves the clone's reattach, as its run, as it is. It signals no process that
/// it has not verified. The progress log only informs it: one that cannot be read is none. A
/// clone that launched, whose progress log, which its launch created before it began, and appends
/// to without a sync, cannot be read, is missing, or was written during another boot of the host,
/// whose crash may have lost its last lines, may have run its workload: a status of the guest's
/// is then attributed as once the start of the clone's sandbox was sent.
///
/// Fails with [`ErrorKind::Other`](crate::ErrorKind::Other) if the sandbox holds no clone
/// binding, whose VM reattaches as any sandbox's does, with
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) if `bound` is zero or longer than
/// [`MAX_TIMEOUT`](crate::host::launch::MAX_TIMEOUT), and for an invalid `runtime`, before it
/// writes anything, and as [`Lease::of`] and [`supervisor::reattach`] fail for records that are
/// inconsistent, which leave the clone's VM as it was, and the record of the failure of an
/// interrupted launch, or the one that replaced a record that could not be read, if the reattach
/// wrote it, pending. Fails with [`ErrorKind::Io`](crate::ErrorKind::Io) if it cannot record the
/// clone's failure, or make the record of an earlier one durable, before it would end, or forget,
/// anything of the clone's VM, which it then leaves as it found it, running or not, with the
/// journal of its launch or its runtime record, and the lease, for a later reattach.
///
/// [`CloneRecords`]: super::CloneRecords
pub fn reattach_clone<C: CloneControl>(
    locked: &mut LockedStore,
    runtime: &RuntimeIdentity,
    control: &mut C,
    deadline: Instant,
    bound: Duration,
) -> Result<ReattachedClone<C::Session>> {
    reattach_clone_with(
        locked,
        runtime,
        control,
        deadline,
        bound,
        |locked, runtime, exited| supervisor::reattach_reporting(locked, runtime, exited),
    )
}

/// Reattaches to a clone as [`reattach_clone`] does, reattaching to its VM with `reattach`, which
/// is [`supervisor::reattach_reporting`] outside tests, and which calls its last argument with
/// the runtime record of a VMM that exited before it clears the record, and fails, keeping the
/// record, if that fails.
pub(super) fn reattach_clone_with<C: CloneControl>(
    locked: &mut LockedStore,
    runtime: &RuntimeIdentity,
    control: &mut C,
    deadline: Instant,
    bound: Duration,
    reattach: impl FnOnce(
        &mut LockedStore,
        &RuntimeIdentity,
        &dyn Fn(&RuntimeRecord) -> Result<()>,
    ) -> Result<Reattached>,
) -> Result<ReattachedClone<C::Session>> {
    reattach_clone_syncing(
        locked, runtime, control, deadline, bound, &sync_dir, reattach,
    )
}

/// Reattaches to a clone as [`reattach_clone_with`] does, syncing the sandbox's directory, to make
/// the record of an earlier failure durable, with `sync`, which is [`sync_dir`] outside tests.
pub(super) fn reattach_clone_syncing<C: CloneControl>(
    locked: &mut LockedStore,
    runtime: &RuntimeIdentity,
    control: &mut C,
    deadline: Instant,
    bound: Duration,
    sync: &dyn Fn(&Path) -> Result<()>,
    reattach: impl FnOnce(
        &mut LockedStore,
        &RuntimeIdentity,
        &dyn Fn(&RuntimeRecord) -> Result<()>,
    ) -> Result<Reattached>,
) -> Result<ReattachedClone<C::Session>> {
    let started = Instant::now();
    if let Some(problem) = timeout_problem(bound, true) {
        return Err(Error::unsupported(format!(
            "the bound of a clone's termination {problem}"
        )));
    }
    // The reattach fails for an invalid runtime identity before it writes anything.
    runtime.check()?;
    let Some(lease) = Lease::of(locked)? else {
        return Err(Error::other(format!(
            "the sandbox in {} holds no clone binding, and so no clone to reattach to",
            locked.dir().display()
        )));
    };
    let launch = lease.launch();
    let journal = locked
        .journal()?
        .filter(|journal| journal.launch() == launch);
    let runtime_record = locked.runtime()?;
    let committed = runtime_record
        .as_ref()
        .filter(|record| record.launch() == launch);
    // The record of an earlier failure of the clone, or the fault of a record that cannot be read,
    // or of one of another clone's failure, which still tells that the clone launched, and failed
    // for good, though not how.
    let earlier = read_failure_of(&locked.clone_failure_path(), &lease)
        .map_err(unreadable_failure)
        .transpose();
    let logged = read_progress(&locked.clone_progress_path(), launch).unwrap_or(None);
    // The launch of a clone creates its progress log before it begins, and before it records a
    // failure, and appends to it without a sync, so the log of a clone that launched tells how
    // far the clone got for sure only if it can be read, and was written during the current boot
    // of the host, whose crash would have ended the clone's VM: a crash of the host may have lost
    // the log's last lines, and a workload may then have run in the clone. A log that can be read
    // tells that the clone launched as well as the store's records do, which a stop of its VM
    // clears, and a release alone removes it.
    let launched =
        journal.is_some() || committed.is_some() || earlier.is_some() || logged.is_some();
    let certain = logged.is_some_and(|logged| logged.is_certain(BootId::current().ok()));
    let uncertain = launched && !certain;
    let logged = logged.map(|logged| logged.progress);
    let progress = progress_of(&lease, journal.as_ref(), committed, logged);
    let activation = read_activation(&locked.clone_activation_path(), &lease);
    // A failure that the reattach records has the transaction that the clone's activation record
    // names, while the record of an earlier failure keeps its own.
    let transaction = activation
        .as_ref()
        .ok()
        .and_then(|(_, record)| record.restore().map(|restore| restore.transaction));
    let recorder = Recorder::new(lease.clone(), transaction, locked).uncertain(uncertain);
    let fresh = earlier.is_none();
    // A record of an earlier failure may be in place without being durable, as a write whose sync
    // of the sandbox's directory failed leaves it: the reattach makes it durable before it ends,
    // or forgets, anything that tells that the clone launched, as it records a failure of its own.
    if let Some(Ok(_)) = &earlier
        && (journal.is_some() || committed.is_some())
    {
        sync(locked.dir()).map_err(|error| {
            unrecorded(Error::io(format!(
                "the record of the clone's failure could not be made durable before its \
                 teardown: {}",
                describe(&error)
            )))
        })?;
    }
    // A record that cannot be read, or that is another clone's, is replaced by the reattach's own
    // failure, with its teardown pending, before the reattach ends or forgets anything of the
    // clone's launch: the record keeps how far the clone got, which the launch's journal and
    // runtime record hold, and the reattach removes, for a reattach after a crash of this caller,
    // which finishes it. Without them, the reattach removes nothing that the record would keep, and
    // a later reattach finds the clone as this one does.
    if let Some(Err(fault)) = &earlier
        && (journal.is_some() || committed.is_some())
    {
        recorder
            .pending(
                Stage::Reattach,
                progress,
                (fault.evidence, None),
                &fault.error,
            )
            .map_err(unrecorded)?;
    }
    // A launch that the journal names, which no runtime record supersedes, and whose failure the
    // sandbox does not record, was interrupted by a crash of its caller: its failure is recorded
    // before the recovery ends it, so that a crash of this caller during the recovery leaves the
    // record. A journal that the runtime record of its launch supersedes, which the launch's
    // commit may leave, is no failure: the recovery removes it.
    if fresh && journal.is_some() && runtime_record.is_none() {
        recorder
            .pending(
                progress.stage(),
                progress,
                (Evidence::LostStatus, None),
                &interrupted(progress),
            )
            .map_err(unrecorded)?;
    }
    // So is the failure of a VM whose launch committed, and which exited, before the reattach
    // clears its runtime record, as the outcome report that the recorder reads then tells, which
    // the rest of the reattach takes as it found it.
    let exiting = |_: &RuntimeRecord| {
        if fresh && let Some((fault, status)) = exit_fault(&recorder, progress, true) {
            let evidence = (fault.evidence, status);
            return recorder
                .pending(progress.stage(), progress, evidence, &fault.error)
                .map_err(unrecorded);
        }
        Ok(())
    };
    let reattached = reattach(locked, runtime, &exiting)?;
    let failed = |fault: Fault, stage: Stage, teardown: Teardown| {
        let fault = Fault::new(fault.evidence, unended(fault.error, &teardown));
        Ok(ReattachedClone::Failed(
            recorder.record(stage, progress, fault, teardown),
        ))
    };
    if let Some(earlier) = earlier {
        let teardown = match reattached {
            Reattached::Live(vmm) | Reattached::Mismatched { vmm, .. } => {
                stopped(vmm.terminate(locked, bound), bound)
            }
            Reattached::Exited {
                recovery: recovery @ (Recovery::Exited | Recovery::Terminated(_)),
                ..
            } => Teardown::of(Ok(recovery)),
            Reattached::Exited {
                record: Some(_), ..
            } => Teardown::Exited,
            // Nothing of the clone remains: a teardown that was not resolved is now.
            Reattached::Exited { record: None, .. } => match &earlier {
                Ok(earlier) => match earlier.teardown {
                    Teardown::Unresolved(_) | Teardown::Pending => Teardown::Exited,
                    ref teardown => teardown.clone(),
                },
                // A VM of the clone ran once its launch spawned it, and so did one whose report the
                // sandbox holds once the launch began, as in `exit_fault`, though no record kept
                // its spawn.
                Err(_)
                    if progress >= Progress::Spawned
                        || (progress > Progress::Planned && recorder.observed().is_some()) =>
                {
                    Teardown::Exited
                }
                Err(_) => Teardown::NoVm,
            },
            Reattached::Indeterminate(reason) => Teardown::Unresolved(reason),
        };
        let earlier = match earlier {
            Ok(earlier) => earlier,
            // The record cannot tell how the clone failed: the failure is the reattach's own.
            Err(fault) => return failed(fault, Stage::Reattach, teardown),
        };
        let failure = if earlier.teardown == Teardown::Pending {
            // The crash of the caller during the teardown left the failure pending, which is
            // recorded as the failure would have recorded it once its teardown ended.
            let error = unended(earlier.error(), &teardown);
            recorder.finish(earlier, error, teardown)
        } else {
            recorder.resolve(earlier, teardown)
        };
        return Ok(ReattachedClone::Failed(failure));
    }
    match reattached {
        Reattached::Live(vmm) => {
            let checked = activation.and_then(|activation| {
                check_activation(locked, &lease, &vmm, control, activation, deadline, started)
            });
            match checked {
                Ok(session) => Ok(ReattachedClone::Live {
                    vmm: Box::new(vmm),
                    session,
                }),
                Err(fault) => {
                    let evidence = (fault.evidence, None);
                    recorder
                        .pending(Stage::Reattach, progress, evidence, &fault.error)
                        .map_err(unrecorded)?;
                    let teardown = stopped(vmm.terminate(locked, bound), bound);
                    failed(fault, Stage::Reattach, teardown)
                }
            }
        }
        Reattached::Mismatched { vmm, differences } => {
            let fault = Fault::new(Evidence::RuntimeMismatch, mismatched(&differences));
            let evidence = (fault.evidence, None);
            recorder
                .pending(Stage::Reattach, progress, evidence, &fault.error)
                .map_err(unrecorded)?;
            let teardown = stopped(vmm.terminate(locked, bound), bound);
            failed(fault, Stage::Reattach, teardown)
        }
        Reattached::Exited {
            recovery: recovery @ (Recovery::Exited | Recovery::Terminated(_)),
            ..
        } => {
            let evidence = match recovery {
                Recovery::Terminated(_) => Evidence::ForcedKill,
                _ => Evidence::LostStatus,
            };
            let fault = Fault::new(evidence, interrupted(progress));
            failed(fault, progress.stage(), Teardown::of(Ok(recovery)))
        }
        Reattached::Exited { record, .. } => {
            match exit_fault(&recorder, progress, record.is_some()) {
                Some((fault, _)) => failed(fault, progress.stage(), Teardown::Exited),
                // A report is of the clone's VM only once its launch began, as in `exit_fault`.
                None => Ok(ReattachedClone::Ended {
                    progress,
                    report: (progress > Progress::Planned)
                        .then(|| recorder.observed().cloned())
                        .flatten(),
                }),
            }
        }
        Reattached::Indeterminate(reason) => {
            if locked.journal()?.is_none() {
                return Ok(ReattachedClone::Indeterminate(reason));
            }
            // The launch that a crash of its caller interrupted cannot be ended yet.
            let fault = Fault::new(Evidence::LostStatus, interrupted(progress));
            failed(fault, progress.stage(), Teardown::Unresolved(reason))
        }
    }
}

/// Returns the fault of the clone whose failures `recorder` records, which got as far as
/// `progress`, and whose VM exited, as OpenVMM's outcome report, which
/// [`Recorder::observed`] reads, tells, with the guest's status, or `None` if the VM ended
/// without a failure: a status of the guest's that is evidence, as [`Recorder::reported`] gives
/// it, or, for a VM that exited `unheld`, whose runtime record the reattach found, as
/// [`Vmm::stop_unless_exited`] keeps that of a VMM that had exited before the stop began, the loss
/// of the clone's status, if no report of the guest's own end tells why.
fn exit_fault(
    recorder: &Recorder,
    progress: Progress,
    unheld: bool,
) -> Option<(Fault, Option<u8>)> {
    // A report is of the clone's VM only once its launch began, which removes the report of an
    // earlier VM of the sandbox before it creates the clone's progress log, as
    // `CloneRecords::read` takes it: a VMM that the launch spawned wrote it, though no record may
    // keep the spawn, if the launch's caller crashed before the journal or the progress log
    // recorded it, and a recovery of the sandbox then removed the journal.
    if progress <= Progress::Planned {
        return None;
    }
    let (evidence, status) = recorder.reported(recorder.observed(), progress);
    let evidence = match evidence {
        Some(evidence) => evidence,
        // Only a VM that exited unheld, whose runtime record the reattach found, ends without
        // telling why: its holder's stop ends one otherwise, and clears the record, but keeps that
        // of one that had exited before the stop began, as `Vmm::stop_unless_exited` does.
        None if unheld && status.is_none() => Evidence::LostStatus,
        None => return None,
    };
    Some((Fault::new(evidence, exited(progress, status)), status))
}

/// Reads the clone's activation record at `path`, which must be the canonical encoding of a
/// record of the restore of the generation that `lease` holds, as the clone's launch wrote it,
/// and returns its bytes and the record, or the fault, the host's, of one that is not.
fn read_activation(
    path: &Path,
    lease: &Lease,
) -> std::result::Result<(Vec<u8>, ActivationRecord), Fault> {
    let bytes = open_regular(path)
        .and_then(|file| read_at_most(file, MAX_RECORD_BYTES as u64, "an activation record"))
        .map_err(|error| {
            Fault::host(
                Error::io(format!(
                    "cannot read the clone's activation record {}",
                    path.display()
                ))
                .with_source(error),
            )
        })?;
    let malformed = |problem: &str| {
        Error::io(format!(
            "the clone's activation record {} {problem}",
            path.display()
        ))
    };
    let record = ActivationRecord::decode(&bytes)
        .map_err(|error| malformed("is malformed").with_source(error))
        .attribute(Evidence::HostFailure)?;
    if record.encode() != bytes {
        return Err(Fault::new(
            Evidence::HostFailure,
            malformed("is not in its canonical encoding, in which the clone's launch wrote it"),
        ));
    }
    if !record
        .restore()
        .is_some_and(|restore| restore.template == lease.template())
    {
        return Err(Fault::new(
            Evidence::HostFailure,
            malformed(&format!(
                "does not restore the generation {} that the clone's lease holds",
                lease.generation()
            )),
        ));
    }
    Ok((bytes, record))
}

/// Checks the clone of `lease`, whose VM `vmm` runs in the sandbox of `locked`, in a new session
/// of `control`, with the sandbox's capability and its activation record, `activation`, by
/// `deadline`, as the reattach that `started` describes, and returns the session in which the
/// clone answered.
fn check_activation<C: CloneControl>(
    locked: &LockedStore,
    lease: &Lease,
    vmm: &Vmm,
    control: &mut C,
    (encoded, record): (Vec<u8>, ActivationRecord),
    deadline: Instant,
    started: Instant,
) -> std::result::Result<C::Session, Fault> {
    let launch = vmm.record().launch();
    if launch != lease.launch() {
        return Err(Fault::new(
            Evidence::HostFailure,
            Error::io(format!(
                "the VM of the sandbox in {} is of the launch {launch}, not of the launch {} that \
                 the clone's lease reserves",
                locked.dir().display(),
                lease.launch()
            )),
        ));
    }
    let capability = locked.read_capability().on_host()?;
    let path = locked.clone_activation_path();
    let session = ActivationSession {
        capability: &capability,
        record: &path,
        expected: ReadyActivation::of(&record),
        encoded,
        deadline,
        timeout: deadline.saturating_duration_since(started),
        since: "its reattach",
    };
    session.activate(control, vmm.endpoint(), vmm.process().reference(), None)
}

/// Returns the teardown of a termination of a clone's VMM within `bound` that ended as `outcome`
/// tells, as [`Vmm::terminate`] returns it.
fn stopped(outcome: Result<StopOutcome>, bound: Duration) -> Teardown {
    match outcome {
        Ok(StopOutcome::Forced) => Teardown::Terminated,
        Ok(StopOutcome::Graceful) => Teardown::Exited,
        Ok(StopOutcome::TimedOut) => Teardown::Unresolved(format!(
            "the clone's VMM did not exit within {bound:?} of its termination"
        )),
        Err(error) => Teardown::Unresolved(describe(&error)),
    }
}

/// Returns `error`, which says so if `teardown` is unresolved.
fn unended(error: Error, teardown: &Teardown) -> Error {
    match teardown {
        Teardown::Unresolved(reason) => amended(
            error,
            &format!("; the clone's VMM could not be ended, and remains for a reattach: {reason}"),
        ),
        _ => error,
    }
}

/// Returns the error of a reattach that could not record the clone's failure, as `error` tells,
/// before it would end, or forget, anything of the clone's VM, which it leaves as it found it.
fn unrecorded(error: Error) -> Error {
    amended(
        error,
        "; the reattach left the clone's VM and its records as it found them, for a later reattach",
    )
}

/// Returns the error of the launch of a clone that got as far as `progress` before a crash of its
/// caller interrupted it.
fn interrupted(progress: Progress) -> Error {
    Error::exited(format!(
        "the launch of the clone was interrupted at its step {progress}, as a crash of its \
         caller interrupts it"
    ))
}

/// Returns the error of a clone's VM that exited once the clone got as far as `progress`, with
/// the guest's `status`, if OpenVMM's outcome report gives one.
fn exited(progress: Progress, status: Option<u8>) -> Error {
    let ending = match status {
        Some(status) => format!("with the guest's status {status}"),
        None => "without an outcome report of the guest's own end".to_owned(),
    };
    Error::exited(format!(
        "the clone's VM exited at its step {progress}, {ending}"
    ))
}

/// Returns the error of a clone's VM that runs under another runtime identity, which differs in
/// `differences`.
fn mismatched(differences: &[IdentityPart]) -> Error {
    let parts: Vec<&str> = differences.iter().map(|part| part.name()).collect();
    Error::new(
        ErrorCode::RuntimeMismatch,
        format!(
            "the clone's VM runs under another runtime identity, which differs in {}, so it is \
             not driven, and is ended through its verified identity",
            parts.join(", ")
        ),
    )
}

/// Returns the fault of a clone whose sandbox holds a record of a failure that cannot be read, or
/// that is another clone's, as `error` tells: the record still shows that the clone failed, so
/// the reattach keeps none of it, with the host's evidence.
fn unreadable_failure(error: Error) -> Fault {
    Fault::host(amended(
        error,
        "; a clone whose sandbox records a failure failed for good, so the reattach keeps no VM \
         of it",
    ))
}
