//! The records that a clone keeps in its sandbox about its launch, beside its activation record:
//! its progress log, [`CLONE_PROGRESS_NAME`](crate::host::store::CLONE_PROGRESS_NAME), and the
//! record of its failure, [`CLONE_FAILURE_NAME`](crate::host::store::CLONE_FAILURE_NAME), which
//! [`CloneRecords::read`] reads with the clone's other records.
//!
//! # The progress log
//!
//! The launch of a clone creates its progress log, in place of one that an earlier launch left,
//! with a first line that names its format, the launch, and the boot of the host during which the
//! launch writes it: `nvx-clone-progress 1 <launch> <boot>`. The launch and its restore sequence
//! then append a line for each step of the clone's [`Progress`] as the clone reaches it, from the
//! spawn of its VMM to the commit of its launch, in order, and sync none of them: the log tells a
//! reattach how far the clone got once the caller of its launch crashed, which a crash of the
//! caller alone loses no write of. A crash of the host, which ends the clone's VM, may lose the
//! log's last lines, so how far the clone got is uncertain, once it launched, by a log that was
//! written during another boot, as by one that is damaged or missing: a workload may have run in
//! the clone. The launch appends the spawn before it returns, although the launch's journal
//! records it too, since a teardown of the launch removes the journal. A step that the next step
//! follows at once, the acknowledgment, which the Activate follows, is appended with that next
//! step, in its one write, so that the log takes a write for the spawn and for each step from the
//! restore-ready event on. A reader ignores a last line that a crash cut short. A sandbox
//! that boots cold writes no log.
//!
//! # The failure record
//!
//! The record of a clone's failure is JSON in format [`FAILURE_FORMAT`], which the failure writes
//! before its teardown, with the teardown [pending](Teardown::Pending), and again once its
//! teardown is known, replacing a record atomically, as the sandbox's store replaces its records,
//! and durably: each write syncs the sandbox's directory once the record is in place, so that the
//! record outlasts a crash of the host, as the clone's binding does. Like the store's records, the
//! record, and the directory that a write syncs, are named by their paths: the directories above
//! the sandbox's must be ones that only the current user, or an administrator, may change, as the
//! [store](crate::host::store) requires of them. The record keeps at most
//! 8192 characters of the failure's message, and 1024 of the reason of an unresolved teardown: of
//! a longer text, as the diagnosis of a caller that fails the clone may be, its first and its last
//! characters, with an ellipsis between them, so that a read, which takes at most 64 KiB of a
//! record, takes every record that a failure writes. A record of another format fails rather
//! than being read. The failure of a VMM that exited before its restore-ready event reads the
//! launch's log back from its end for the line with which OpenVMM reported the error that ended
//! it, which the record's message keeps, and whose time ABI code may attribute the failure, as
//! [the attribution of evidence](Evidence#attribution) describes.
//!
//! A release of the clone's lease removes both, as it removes the clone's other files, so that
//! the records of a clone last as long as its lease, which a failure whose teardown is unresolved
//! keeps.

use std::cell::OnceCell;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::failure::{
    self, CloneFailure, Evidence, FailureRecord, Fault, Outcome, Progress, Stage, Teardown,
    amended, describe,
};
use crate::activation::{Identity, StoreGeneration, TemplateReference, TransactionId};
use crate::error::{Error, ErrorCode, Result};
use crate::host::platform::{self, parse_hex_id};
use crate::host::process::BootId;
use crate::host::report::{self, Report};
use crate::host::store::files::{create_new, sync_dir, write_private};
use crate::host::store::regular::{open_regular, read_at_most, was_removed};
use crate::host::store::{
    CLONE_PROGRESS_NAME, LaunchId, LaunchJournal, OUTCOME_NAME, Recovery, RuntimeRecord,
    SandboxStore,
};
use crate::host::supervisor::Starting;
use crate::host::template::{KeyIdentity, Lease};

/// The format of the record of a clone's failure.
pub const FAILURE_FORMAT: u32 = 1;

/// The first word of the first line of a clone's progress log.
const PROGRESS_MAGIC: &str = "nvx-clone-progress";

/// The format of a clone's progress log, after [`PROGRESS_MAGIC`] in its first line.
const PROGRESS_FORMAT: u32 = 1;

/// The most bytes of a clone's progress log that a read takes: its first line, and a line for each
/// of its steps, take a few hundred.
const MAX_PROGRESS_BYTES: u64 = 4096;

/// The most bytes of the record of a clone's failure that a read takes: more than any record that
/// a failure writes takes, whose texts [`bounded`] bounds, as [`MAX_MESSAGE_CHARS`] and
/// [`MAX_REASON_CHARS`] tell.
const MAX_FAILURE_BYTES: u64 = 64 * 1024;

/// The most characters of the message of a failure's error that the record of the failure keeps,
/// as [`bounded`] keeps them: the diagnosis of a caller that fails a clone, as
/// [`Activated::fail`](super::Activated::fail) and [`Started::fail`](super::Started::fail) take
/// it, may be longer.
const MAX_MESSAGE_CHARS: usize = 8 * 1024;

/// The most characters of the reason of an unresolved teardown that the record of a failure keeps,
/// as [`bounded`] keeps them.
const MAX_REASON_CHARS: usize = 1024;

// JSON takes at most six bytes for a character, as for `\u0001`, and the other fields of a record
// take less than a kilobyte, so that a read takes every record that a failure writes.
const _: () =
    assert!(6 * (MAX_MESSAGE_CHARS + MAX_REASON_CHARS) + 1024 <= MAX_FAILURE_BYTES as usize);

/// What messages call the progress log of a clone.
const PROGRESS_WHAT: &str = "clone progress log";

/// What messages call the record of a clone's failure.
const FAILURE_WHAT: &str = "clone failure record";

/// The bytes of a VMM's log that a failure reads at a time as it looks back from the log's end for
/// the line of OpenVMM's fatal error, which OpenVMM writes as it exits, followed by the error's
/// chain alone, of any length: the chain of a failed CPU check names every difference that the
/// check found.
const FATAL_BLOCK_BYTES: u64 = 64 * 1024;

/// The most characters of the line of OpenVMM's fatal error that the record of a failure keeps.
const MAX_FATAL_CHARS: usize = 1024;

/// The progress log of a clone's launch, open for writing, with the latest step that the clone
/// reached, and the latest that the log records, as the [module](self#the-progress-log)
/// describes.
#[derive(Debug)]
pub(super) struct ProgressLog {
    /// The log, open for writing at its end.
    file: File,
    /// The log's path.
    path: Box<Path>,
    /// The latest step that the clone reached.
    progress: Progress,
    /// The latest step that the log records.
    logged: Progress,
}

impl ProgressLog {
    /// Creates the progress log of the launch `launch` in the sandbox's directory `dir`, in place
    /// of one that an earlier launch left, with its first line, which names the current boot of
    /// the host, and returns it, at [`Progress::Launching`]. It first removes the outcome report
    /// that an earlier VM of the sandbox left, which no acquisition, clearing of a VM's runtime
    /// record, or release removes, so that a report beside the clone's records is the clone's VM's,
    /// as [`CloneRecords::read`] takes it.
    ///
    /// `directory` holds the sandbox's directory open, as the sandbox's lock opened and checked it
    /// through `dir`. The earlier report and log go through it, as a release removes the clone's
    /// files, so that the removals reach the directory that holds the lock, whatever `dir` names
    /// meanwhile. The log is then created at its path, where no file may exist, so that a path that
    /// names another sandbox's directory by then fails the creation if that sandbox holds a log,
    /// rather than replace it.
    pub(super) fn create(
        directory: &platform::Directory,
        dir: &Path,
        launch: &LaunchId,
    ) -> Result<Self> {
        let path = dir.join(CLONE_PROGRESS_NAME);
        let failed = |error: io::Error| {
            Error::io(format!(
                "cannot create the {PROGRESS_WHAT} {}",
                path.display()
            ))
            .with_source(error)
        };
        let boot = BootId::current().map_err(failed)?;
        for name in [OUTCOME_NAME, CLONE_PROGRESS_NAME] {
            directory.remove_file(name).map_err(|error| {
                Error::io(format!("cannot remove {}", dir.join(name).display())).with_source(error)
            })?;
        }
        let mut file = create_new(&path).map_err(failed)?;
        file.write_all(format!("{PROGRESS_MAGIC} {PROGRESS_FORMAT} {launch} {boot}\n").as_bytes())
            .map_err(failed)?;
        Ok(Self {
            file,
            path: path.into(),
            progress: Progress::Launching,
            logged: Progress::Launching,
        })
    }

    /// Returns the latest step that the clone reached.
    pub(super) fn progress(&self) -> Progress {
        self.progress
    }

    /// Notes that the clone reached `step`, unless it reached it, or a later step, already,
    /// without writing it: the log records it with the next step that it writes. It is for a
    /// step that another record holds, as the record of a launch's failure holds the spawn of a
    /// VMM that the launch could not record, or that the next step follows at once.
    pub(super) fn advance(&mut self, step: Progress) {
        self.progress = self.progress.max(step);
    }

    /// Records that the clone reached `step`, unless it reached it, or a later step, already:
    /// appends the line of each step that the clone reached since the latest that the log
    /// records, through `step`, in order, with one write, which nothing syncs.
    pub(super) fn reach(&mut self, step: Progress) -> Result<()> {
        self.advance(step);
        if self.progress <= self.logged {
            return Ok(());
        }
        let lines: String = LOGGED
            .iter()
            .filter(|&&logged| self.logged < logged && logged <= self.progress)
            .map(|logged| format!("{logged}\n"))
            .collect();
        self.file.write_all(lines.as_bytes()).map_err(|error| {
            Error::io(format!(
                "cannot record that the clone reached its step {} in the {PROGRESS_WHAT} {}",
                self.progress,
                self.path.display()
            ))
            .with_source(error)
        })?;
        self.logged = self.progress;
        Ok(())
    }
}

/// The steps that a progress log records after its first line, in order.
const LOGGED: [Progress; 8] = [
    Progress::Spawned,
    Progress::Event,
    Progress::Acknowledged,
    Progress::Activating,
    Progress::Ready,
    Progress::Starting,
    Progress::Started,
    Progress::Committed,
];

/// How the launch of a clone that failed is ended: [`Starting::abandon`], outside tests, which
/// replace it to keep a launch that cannot be ended.
pub(super) type End = for<'a> fn(Starting<'a>) -> Result<Recovery>;

/// Ends `starting` as [`Starting::abandon`] does: how the launch of a clone that failed is ended
/// outside tests.
pub(super) fn abandon(starting: Starting<'_>) -> Result<Recovery> {
    starting.abandon()
}

/// What a clone's failures are recorded with: the clone's lease and the transaction of its
/// restore, the paths, in its sandbox, of the record of its failure, of OpenVMM's outcome report
/// of its VM, and of its VMM's log, whether a workload may have run in the clone whatever its
/// records say, as for a clone whose progress log was damaged, or written during another boot,
/// and the report, once it was read.
#[derive(Debug)]
pub(super) struct Recorder {
    /// The clone's lease.
    lease: Lease,
    /// The transaction ID of the clone's restore.
    transaction: Option<TransactionId>,
    /// The path of the record of the clone's failure.
    failure: PathBuf,
    /// The path of OpenVMM's outcome report of the clone's VM.
    report: PathBuf,
    /// The path of the log of the clone's VMM.
    log: PathBuf,
    /// Whether how far the clone got is uncertain beyond the progress that its records give, so
    /// that a workload may have run in it.
    uncertain: bool,
    /// OpenVMM's outcome report of the clone's VM, as [`observed`](Self::observed) first read it.
    observed: OnceCell<Option<Report>>,
}

impl Recorder {
    /// Returns the recorder of the failures of the clone of `lease`, with the transaction of its
    /// restore, `transaction`, in the sandbox of `sandbox`.
    pub(super) fn new(
        lease: Lease,
        transaction: Option<TransactionId>,
        sandbox: &SandboxStore,
    ) -> Self {
        Self {
            lease,
            transaction,
            failure: sandbox.clone_failure_path(),
            report: sandbox.outcome_path(),
            log: sandbox.log_path(),
            uncertain: false,
            observed: OnceCell::new(),
        }
    }

    /// Returns the recorder, for a clone whose progress is uncertain beyond what its records
    /// give, if `uncertain`: a status of the guest's is then attributed as once a workload may
    /// have run, as [`reported`](Self::reported) does.
    pub(super) fn uncertain(self, uncertain: bool) -> Self {
        Self { uncertain, ..self }
    }

    /// Returns OpenVMM's outcome report of the clone's VM, or `None` if it is missing or
    /// malformed, as the recorder first read it, the first time that it needed it: once the VM
    /// has ended, whose report is final then, so that a read that fails later, as a read that
    /// another process's open of the report refuses may, takes nothing back of what the first
    /// one showed. A recorder serves one step, a launch that fails or a reattach, which reads the
    /// report only once the clone's VM ended, or its teardown did.
    pub(super) fn observed(&self) -> Option<&Report> {
        self.observed
            .get_or_init(|| report::read(&self.report))
            .as_ref()
    }

    /// Returns the guest's status in OpenVMM's outcome report `report` of the clone's VM, and the
    /// evidence that the status alone is once the clone got as far as `progress`, as
    /// [`failure::reported`] gives them, or as once a workload may have run, if the clone's
    /// progress is [uncertain](Self::uncertain).
    pub(super) fn reported(
        &self,
        report: Option<&Report>,
        progress: Progress,
    ) -> (Option<Evidence>, Option<u8>) {
        let progress = if self.uncertain {
            progress.max(Progress::Starting)
        } else {
            progress
        };
        failure::reported(report, progress)
    }

    /// Records the failure of the clone at `stage`, once it got as far as `progress`, with
    /// `fault`, whose teardown ended as `teardown` tells, as [the records of
    /// failures](FailureRecord#records) describe, attributed as [`attribute`](Self::attribute)
    /// does, and returns it. A failure that cannot be recorded says so.
    pub(super) fn record(
        &self,
        stage: Stage,
        progress: Progress,
        fault: Fault,
        teardown: Teardown,
    ) -> CloneFailure {
        let (evidence, status, error) = self.attribute(progress, fault, &teardown);
        let record = FailureRecord::new(
            &self.lease,
            self.transaction,
            (stage, progress),
            evidence,
            status,
            teardown,
            &error,
        );
        self.write(error, record)
    }

    /// Finishes `earlier`, the record of a failure whose teardown a crash of the caller left
    /// [pending](Teardown::Pending), once its teardown ended as `teardown` tells, with the
    /// failure's `error`, as the failure would have recorded it then: attributed from its
    /// evidence, as [`attribute`](Self::attribute) does, keeping its lease, generation, launch,
    /// transaction, stage, and progress, and the guest's status that it holds, if the outcome
    /// report gives none now. Writes the record in place of the earlier one, and returns the
    /// failure.
    pub(super) fn finish(
        &self,
        earlier: FailureRecord,
        error: Error,
        teardown: Teardown,
    ) -> CloneFailure {
        let fault = Fault::new(earlier.evidence, error);
        let (evidence, status, error) = self.attribute(earlier.progress, fault, &teardown);
        let record = FailureRecord {
            evidence,
            status: status.or(earlier.status),
            outcome: evidence.outcome(),
            teardown,
            code: error.code(),
            message: describe(&error),
            ..earlier
        };
        self.write(error, record)
    }

    /// Returns the evidence, the guest's status, and the error of the failure of the clone with
    /// `fault`, once it got as far as `progress`, whose teardown ended as `teardown` tells: the
    /// loss of a VMM that exited before its restore-ready event takes the line of OpenVMM's fatal
    /// error that the VMM's log ends with, as [`diagnosed`](Self::diagnosed) does, and the
    /// evidence of a status of the guest's, which OpenVMM's outcome report gives once the VMM has
    /// exited, as [`observed`](Self::observed) reads it, and as [`reported`](Self::reported)
    /// gives it, prevails over the fault's, unless the fault's is the template's, as
    /// [`failure::prevailing`] decides.
    fn attribute(
        &self,
        progress: Progress,
        fault: Fault,
        teardown: &Teardown,
    ) -> (Evidence, Option<u8>, Error) {
        let fault = self.diagnosed(progress, fault);
        let report = match teardown {
            Teardown::NoVm => None,
            _ => self.observed(),
        };
        let (reported, status) = self.reported(report, progress);
        let evidence = failure::prevailing(fault.evidence, reported);
        (evidence, status, fault.error)
    }

    /// Returns `fault`, of the clone that got as far as `progress`, which is the loss of a VMM
    /// that exited before its restore-ready event, with the line with which OpenVMM reported the
    /// error that ended the VMM after its message, if the VMM's log holds one, as
    /// [`read_fatal`] reads it, and the message does not hold it yet, as the message of a failure
    /// that a reattach resolves may, and with the line's evidence, if it is evidence, as [the
    /// attribution of evidence](Evidence#attribution) describes. Returns any other fault as it is.
    fn diagnosed(&self, progress: Progress, fault: Fault) -> Fault {
        if fault.evidence != Evidence::LostStatus || progress >= Progress::Event {
            return fault;
        }
        let Some(line) = read_fatal(&self.log) else {
            return fault;
        };
        let evidence = failure::fatal_evidence(&line).unwrap_or(fault.evidence);
        let reported = format!("; OpenVMM reported: {line}");
        let error = if describe(&fault.error).contains(&reported) {
            fault.error
        } else {
            amended(fault.error, &reported)
        };
        Fault::new(evidence, error)
    }

    /// Records the failure of the clone at `stage`, once it got as far as `progress`, with
    /// `evidence`, the guest's `status`, if the failure knows it already, and `error`, before the
    /// teardown that follows it, whose record is [`Teardown::Pending`], so that a crash of the
    /// caller during the teardown leaves the record, which a reattach finishes, as
    /// [`reattach_clone`](super::reattach_clone) describes. The record that the failure writes
    /// once its teardown ended replaces it.
    ///
    /// Fails if the record cannot be written, durably, as [`write_failure`] writes it: the caller
    /// then removes nothing that tells that the clone launched, the journal of its launch or the
    /// runtime record of its VM, which keep its lease, so that a reattach still finds that the
    /// clone failed, though not how, if a crash of the caller comes before the failure is
    /// recorded.
    pub(super) fn pending(
        &self,
        stage: Stage,
        progress: Progress,
        (evidence, status): (Evidence, Option<u8>),
        error: &Error,
    ) -> Result<()> {
        let record = FailureRecord::new(
            &self.lease,
            self.transaction,
            (stage, progress),
            evidence,
            status,
            Teardown::Pending,
            error,
        );
        write_failure(&self.failure, &record).map_err(|failed| {
            Error::io(format!(
                "the failure could not be recorded before its teardown: {}",
                describe(&failed)
            ))
        })
    }

    /// Resolves `earlier`, the record of a failure that its teardown left unresolved, with
    /// `teardown`, which ended the clone's VMM, keeping its stage, evidence, outcome, code, and
    /// message, unless what the VMM left as it ended tells more: the line of OpenVMM's fatal
    /// error, for the loss of a VMM that exited before its restore-ready event, as
    /// [`diagnosed`](Self::diagnosed) adds it, and the guest's status in OpenVMM's outcome
    /// report, which the record does not hold, whose evidence, as [`reported`](Self::reported)
    /// gives it, prevails as [`failure::prevailing`] decides, as in [`record`](Self::record).
    /// Writes the record in place of the earlier one, if they differ, and returns the failure.
    pub(super) fn resolve(&self, mut earlier: FailureRecord, teardown: Teardown) -> CloneFailure {
        let error = earlier.error();
        if earlier.teardown == teardown {
            return CloneFailure::new(error, earlier);
        }
        let fault = self.diagnosed(earlier.progress, Fault::new(earlier.evidence, error));
        let mut evidence = fault.evidence;
        if earlier.status.is_none() && teardown != Teardown::NoVm {
            let (reported, status) = self.reported(self.observed(), earlier.progress);
            evidence = failure::prevailing(evidence, reported);
            earlier.status = status;
        }
        if evidence != earlier.evidence {
            earlier.evidence = evidence;
            earlier.outcome = evidence.outcome();
        }
        earlier.message = describe(&fault.error);
        earlier.teardown = teardown;
        self.write(fault.error, earlier)
    }

    /// Writes `record`, the record of the failure of `error`, in place of the sandbox's record, and
    /// returns the failure, with the record as the sandbox keeps it, as [`kept`] returns it, whose
    /// error says so if the record could not be written.
    pub(super) fn write(&self, error: Error, record: FailureRecord) -> CloneFailure {
        let record = kept(record);
        let error = match write_failure(&self.failure, &record) {
            Ok(()) => error,
            Err(failed) => amended(
                error,
                &format!("; the failure could not be recorded: {}", describe(&failed)),
            ),
        };
        CloneFailure::new(error, record)
    }
}

/// What the launch of a clone keeps to record how far the clone got and how it failed: the
/// [`Recorder`] of its failures, its progress log, and how a failure ends the launch.
#[derive(Debug)]
pub(super) struct Tracker {
    /// The recorder of the clone's failures.
    recorder: Recorder,
    /// The clone's progress log.
    log: ProgressLog,
    /// How a failure ends the launch.
    end: End,
}

impl Tracker {
    /// Returns the tracker of the launch of a clone whose failures `recorder` records, with its
    /// progress `log`, and the end of a launch that fails, `end`.
    pub(super) fn new(recorder: Recorder, log: ProgressLog, end: End) -> Self {
        Self { recorder, log, end }
    }

    /// Notes that the clone reached `step`, as [`ProgressLog::advance`] does, without writing it.
    pub(super) fn advance(&mut self, step: Progress) {
        self.log.advance(step);
    }

    /// Records that the clone reached `step` in its progress log, as [`ProgressLog::reach`] does,
    /// and returns the fault of a failure to record it, the host's.
    pub(super) fn reach(&mut self, step: Progress) -> std::result::Result<(), Fault> {
        self.log.reach(step).map_err(Fault::host)
    }

    /// Records that the clone reached `step` in its progress log if it can: a step that another
    /// record holds, such as the commit, which the runtime record holds while the VM runs.
    pub(super) fn note(&mut self, step: Progress) {
        let _ = self.log.reach(step);
    }

    /// Records the failure of the clone at `stage` with `evidence` and `error` before the teardown
    /// that follows it, once the clone got as far as its progress, as [`Recorder::pending`] does,
    /// and fails as it fails.
    pub(super) fn pending(&self, stage: Stage, evidence: Evidence, error: &Error) -> Result<()> {
        self.recorder
            .pending(stage, self.log.progress(), (evidence, None), error)
    }

    /// Ends `starting`, the launch of the clone, which failed at `stage` with `fault`, as `end`
    /// does, and records the failure before the teardown, as [`pending`](Self::pending) does, and
    /// with the teardown's outcome, as [`record`](Self::record) does. The error of a launch that
    /// could not be ended says so, and that its journal remains for recovery.
    ///
    /// A failure that cannot be recorded before the teardown ends the clone's VMM through its
    /// handle, and keeps the launch's journal, as [`Starting::end_keeping_journal`] does, and with
    /// it the lease, until a reattach ends the launch: the teardown is unresolved, so that a crash
    /// of the caller before the failure is recorded leaves a launch that a reattach records as
    /// failed, rather than a clone that ended.
    pub(super) fn fail(&self, starting: Starting<'_>, stage: Stage, fault: Fault) -> CloneFailure {
        let teardown = match self.pending(stage, fault.evidence, &fault.error) {
            Ok(()) => Teardown::of((self.end)(starting)),
            Err(unrecorded) => {
                let ended = if starting.end_keeping_journal() {
                    "the clone's VMM ended through its handle"
                } else {
                    "the clone's VMM did not exit once it was ended through its handle"
                };
                Teardown::Unresolved(format!("{ended}, but {unrecorded}"))
            }
        };
        let fault = match &teardown {
            Teardown::Unresolved(reason) => Fault::new(
                fault.evidence,
                amended(
                    fault.error,
                    &format!("; the launch could not be ended, and remains for recovery: {reason}"),
                ),
            ),
            _ => fault,
        };
        self.record(stage, fault, teardown)
    }

    /// Records the failure of the clone at `stage` with `fault`, whose teardown ended as
    /// `teardown` tells, once the clone got as far as its progress, as [`Recorder::record`]
    /// does, and returns it.
    pub(super) fn record(&self, stage: Stage, fault: Fault, teardown: Teardown) -> CloneFailure {
        self.recorder
            .record(stage, self.log.progress(), fault, teardown)
    }
}

/// What a clone's progress log records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Logged {
    /// The latest step that the log records.
    pub(super) progress: Progress,
    /// The boot of the host during which the launch wrote the log, as its first line names it,
    /// unless a crash cut that line short.
    pub(super) boot: Option<BootId>,
}

impl Logged {
    /// Returns whether the log tells for sure how far the clone got: whether the launch wrote it
    /// during the boot `current`, the current boot of the host, if it is known, so that no crash
    /// of the host lost a line of it since, as [the module](self#the-progress-log) describes.
    pub(super) fn is_certain(&self, current: Option<BootId>) -> bool {
        self.boot.is_some() && self.boot == current
    }
}

/// Reads the progress log at `path` of the launch `launch`, and returns what it records, or
/// `None` if it does not exist.
///
/// Fails with [`ErrorKind::Io`](crate::ErrorKind::Io) for a log that cannot be read, is not a
/// private regular file, names another launch or format, or no boot, or records other lines than
/// its steps, in order, besides a last line that a crash cut short.
pub(super) fn read_progress(path: &Path, launch: &LaunchId) -> Result<Option<Logged>> {
    let Some(bytes) = read_private(path, PROGRESS_WHAT, MAX_PROGRESS_BYTES)? else {
        return Ok(None);
    };
    let malformed =
        |problem: String| Error::io(format!("the {PROGRESS_WHAT} {} {problem}", path.display()));
    let text = String::from_utf8(bytes).map_err(|_| malformed("is not UTF-8".to_owned()))?;
    // A line without its newline is one that a crash cut short.
    let mut lines = text
        .split_inclusive('\n')
        .filter_map(|line| line.strip_suffix('\n'));
    let header = format!("{PROGRESS_MAGIC} {PROGRESS_FORMAT} {launch} ");
    let boot = match lines.next() {
        Some(line) => match line
            .strip_prefix(&header)
            .and_then(|boot| boot.parse::<BootId>().ok())
        {
            Some(boot) => boot,
            None => {
                return Err(malformed(format!(
                    "begins with {line:?}, not with the first line of the launch {launch}, \
                     {header:?} followed by the identity of a boot"
                )));
            }
        },
        // A log that a crash cut short before its first line ended records the launch alone.
        None => {
            return Ok(Some(Logged {
                progress: Progress::Launching,
                boot: None,
            }));
        }
    };
    let mut progress = Progress::Launching;
    for line in lines {
        match LOGGED.iter().find(|step| step.as_str() == line) {
            Some(&step) if step > progress => progress = step,
            _ => {
                return Err(malformed(format!(
                    "records {line:?} after the step {progress}, which is not a later step"
                )));
            }
        }
    }
    Ok(Some(Logged {
        progress,
        boot: Some(boot),
    }))
}

/// Returns how far the clone of `lease` got, by the latest step that its progress log records,
/// `logged`, and by the store's records of its launch: its `journal`, which records the spawn of
/// its VMM, and its `runtime` record, which records the commit of its launch.
pub(super) fn progress_of(
    lease: &Lease,
    journal: Option<&LaunchJournal>,
    runtime: Option<&RuntimeRecord>,
    logged: Option<Progress>,
) -> Progress {
    let launch = lease.launch();
    let recorded = if runtime.is_some_and(|record| record.launch() == launch) {
        Some(Progress::Committed)
    } else {
        journal
            .filter(|journal| journal.launch() == launch)
            .map(|journal| match journal.process() {
                Some(_) => Progress::Spawned,
                None => Progress::Launching,
            })
    };
    [logged, recorded]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(Progress::Planned)
}

/// The record of a clone's failure, as JSON.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Recorded {
    /// [`FAILURE_FORMAT`].
    format: u32,
    /// The identity of the template store that leased the clone's generation.
    store: String,
    /// The identity of the clone's lease.
    lease: String,
    /// The leased generation.
    generation: u64,
    /// The identity of the leased generation's key.
    key: String,
    /// The launch that the lease reserves.
    launch: String,
    /// The transaction ID of the clone's restore, in hexadecimal, once the plan drew it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transaction: Option<String>,
    /// The stage at which the clone failed.
    stage: Stage,
    /// How far the clone got.
    progress: Progress,
    /// The evidence of the failure.
    evidence: Evidence,
    /// The guest's status, if OpenVMM's outcome report has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    status: Option<u8>,
    /// The original outcome of the failure.
    outcome: Outcome,
    /// How the teardown ended, as [`Teardown::as_str`] spells it.
    teardown: String,
    /// Why the teardown is unresolved, if it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unresolved: Option<String>,
    /// The code of the failure's error, as [`ErrorCode::as_str`] spells it.
    code: String,
    /// The message of the failure's error.
    message: String,
}

impl Recorded {
    /// Returns the JSON form of `record`, as the sandbox keeps it, with texts that [`kept`]
    /// bounds.
    fn new(record: &FailureRecord) -> Self {
        Self {
            format: FAILURE_FORMAT,
            store: record.store.to_string(),
            lease: record.lease.to_string(),
            generation: record.template.generation.get().get(),
            key: KeyIdentity::from(record.template.key).to_string(),
            launch: record.launch.to_string(),
            transaction: record
                .transaction
                .map(|transaction| transaction.to_string()),
            stage: record.stage,
            progress: record.progress,
            evidence: record.evidence,
            status: record.status,
            outcome: record.outcome,
            teardown: record.teardown.as_str().to_owned(),
            unresolved: match &record.teardown {
                Teardown::Unresolved(reason) => Some(bounded(reason, MAX_REASON_CHARS)),
                _ => None,
            },
            code: record.code.as_str().to_owned(),
            message: bounded(&record.message, MAX_MESSAGE_CHARS),
        }
    }

    /// Checks the record, and returns it, or says why it is malformed.
    fn into_record(self) -> std::result::Result<FailureRecord, String> {
        let field =
            |what: &str, error: Error| format!("its {what} is invalid: {}", error.message());
        let teardown = match (self.teardown.as_str(), self.unresolved) {
            ("no-vm", None) => Teardown::NoVm,
            ("exited", None) => Teardown::Exited,
            ("terminated", None) => Teardown::Terminated,
            ("pending", None) => Teardown::Pending,
            ("unresolved", Some(reason)) => Teardown::Unresolved(reason),
            (teardown, _) => {
                return Err(format!(
                    "its teardown, {teardown:?}, is not one that a failure records, or names a \
                     reason only when it is unresolved"
                ));
            }
        };
        let transaction = match self.transaction {
            None => None,
            Some(digits) => Some(TransactionId::from_bytes(
                parse_hex_id(&digits).ok_or_else(|| {
                    format!("its transaction, {digits:?}, is not 32 hexadecimal digits")
                })?,
            )),
        };
        let key: KeyIdentity = self.key.parse().map_err(|error| field("key", error))?;
        // A later build may record a code that this one does not know, which is a failure all
        // the same.
        let code = ErrorCode::ALL
            .iter()
            .copied()
            .find(|code| code.as_str() == self.code)
            .unwrap_or(ErrorCode::Other);
        Ok(FailureRecord {
            store: self.store.parse().map_err(|error| field("store", error))?,
            lease: self.lease.parse().map_err(|error| field("lease", error))?,
            template: TemplateReference {
                generation: NonZeroU64::new(self.generation)
                    .map(StoreGeneration::new)
                    .ok_or("it names store generation 0, which no generation has")?,
                key: Identity::from(key),
            },
            launch: LaunchId::try_from(self.launch)
                .map_err(|problem| format!("its launch is invalid: {problem}"))?,
            transaction,
            stage: self.stage,
            progress: self.progress,
            evidence: self.evidence,
            status: self.status,
            outcome: self.outcome,
            teardown,
            code,
            message: self.message,
        })
    }
}

/// Replaces the record of a clone's failure at `path` with `record`, as the sandbox keeps it, as
/// [`kept`] returns it, atomically, and durably: once the record is in place, it syncs the
/// record's directory, as [`sync_dir`] does, and fails if the sync does, though the record is in
/// place then.
pub(super) fn write_failure(path: &Path, record: &FailureRecord) -> Result<()> {
    write_failure_with(path, record, &sync_dir)
}

/// Replaces the record of a clone's failure as [`write_failure`] does, syncing its directory with
/// `sync`, which tests observe, and fail.
fn write_failure_with(
    path: &Path,
    record: &FailureRecord,
    sync: &dyn Fn(&Path) -> Result<()>,
) -> Result<()> {
    let mut text = serde_json::to_vec_pretty(&Recorded::new(record)).map_err(|error| {
        Error::other(format!("cannot encode the {FAILURE_WHAT}")).with_source(error)
    })?;
    text.push(b'\n');
    write_private(path, &text)?;
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => sync(dir),
        _ => sync(Path::new(".")),
    }
}

/// Returns `record` as the sandbox keeps it: with at most [`MAX_MESSAGE_CHARS`] characters of its
/// message, and [`MAX_REASON_CHARS`] of the reason of its teardown, if it is unresolved, as
/// [`bounded`] keeps them.
fn kept(mut record: FailureRecord) -> FailureRecord {
    record.message = bounded(&record.message, MAX_MESSAGE_CHARS);
    if let Teardown::Unresolved(reason) = &mut record.teardown {
        *reason = bounded(reason, MAX_REASON_CHARS);
    }
    record
}

/// Returns `text`, or, if it has more than `max` characters, `max` of them: its first ones and its
/// last ones, about as many of each, with an ellipsis between them in place of those that it
/// leaves out, so that the message of an error keeps what it begins with, and the messages of its
/// sources, which end it.
fn bounded(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_owned();
    }
    let first = (max - 1) / 2;
    let last = max - 1 - first;
    text.chars()
        .take(first)
        .chain(std::iter::once('…'))
        .chain(text.chars().skip(count - last))
        .collect()
}

/// Reads the record of a clone's failure at `path`, or returns `None` if it does not exist.
///
/// Fails with [`ErrorCode::FormatUnsupported`] for a record of another format than
/// [`FAILURE_FORMAT`], and with [`ErrorKind::Io`](crate::ErrorKind::Io) for one that cannot be
/// read, is not a private regular file, or is malformed.
pub(super) fn read_failure(path: &Path) -> Result<Option<FailureRecord>> {
    let Some(bytes) = read_private(path, FAILURE_WHAT, MAX_FAILURE_BYTES)? else {
        return Ok(None);
    };
    let malformed = || format!("the {FAILURE_WHAT} {} is malformed", path.display());
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| Error::io(malformed()).with_source(error))?;
    match value.get("format") {
        Some(format) if format.as_u64() == Some(FAILURE_FORMAT.into()) => {}
        Some(format) => {
            return Err(Error::new(
                ErrorCode::FormatUnsupported,
                format!(
                    "the {FAILURE_WHAT} {} has the unsupported format {format}; this build reads \
                     format {FAILURE_FORMAT} only",
                    path.display()
                ),
            ));
        }
        None => return Err(Error::io(format!("{}: it has no format", malformed()))),
    }
    let recorded: Recorded =
        serde_json::from_value(value).map_err(|error| Error::io(malformed()).with_source(error))?;
    recorded
        .into_record()
        .map(Some)
        .map_err(|problem| Error::io(format!("{}: {problem}", malformed())))
}

/// Reads the record of the failure of the clone of `lease` at `path`, as [`read_failure`] reads
/// it, or returns `None` if it does not exist.
///
/// Fails as [`read_failure`] does, and, as for a record that is malformed, with
/// [`ErrorKind::Io`](crate::ErrorKind::Io) for a record of another clone's failure, whose template
/// store, lease, leased generation, its key, or launch is not the lease's: it tells nothing of
/// this clone, and a [quarantine](CloneFailure::quarantine) of its failure would tombstone the
/// record's generation, which may not be the one that this clone leases.
pub(super) fn read_failure_of(path: &Path, lease: &Lease) -> Result<Option<FailureRecord>> {
    let Some(record) = read_failure(path)? else {
        return Ok(None);
    };
    let leased = lease.template();
    let differences: Vec<&str> = [
        (record.store != lease.store(), "the template store"),
        (record.lease != lease.id(), "the lease"),
        (
            record.template.generation != leased.generation,
            "the leased generation",
        ),
        (
            record.template.key != leased.key,
            "the key of the leased generation",
        ),
        (record.launch != *lease.launch(), "the launch"),
    ]
    .into_iter()
    .filter_map(|(differs, what)| differs.then_some(what))
    .collect();
    let Some((last, others)) = differences.split_last() else {
        return Ok(Some(record));
    };
    let differences = if others.is_empty() {
        (*last).to_owned()
    } else {
        format!("{} and {last}", others.join(", "))
    };
    Err(Error::io(format!(
        "the {FAILURE_WHAT} {} belongs to another clone than the one of the lease {}, which the \
         sandbox's clone binding records: they differ in {differences}",
        path.display(),
        lease.id(),
    )))
}

/// Reads the private regular file at `path`, the clone's `description`, of at most `limit` bytes,
/// or returns `None` if it does not exist, or was removed as this opened it, as [`was_removed`]
/// tells.
fn read_private(path: &Path, description: &str, limit: u64) -> Result<Option<Vec<u8>>> {
    let failed = |error: io::Error| {
        Error::io(format!("cannot read the {description} {}", path.display())).with_source(error)
    };
    let file = match open_regular(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound || was_removed(path, &error) => {
            return Ok(None);
        }
        Err(error) => return Err(failed(error)),
    };
    if let Some(problem) = platform::private_file_problem(&file).map_err(failed)? {
        return Err(Error::io(format!(
            "the {description} {} {problem}",
            path.display()
        )));
    }
    read_at_most(file, limit, &format!("the {description}"))
        .map(Some)
        .map_err(failed)
}

/// Returns the last line with which OpenVMM reported the error that ended it in the VMM's log at
/// `path`, cut to [`MAX_FATAL_CHARS`], or none if the log holds none, or cannot be read: the line
/// that leads OpenVMM's fatal error, then the error's stable code, if it has one, as [the
/// attribution of evidence](Evidence#attribution) describes. The line is looked for back from the
/// log's end, as [`last_line_start`] reads it, so that the error's chain, which follows the line,
/// may be of any length. If the line leads with the code of a forward that OpenVMM cannot bind,
/// [`failure::FORWARD_BIND_CODE`], the first cause of the error's chain that leads with the code
/// too, which names the forward, follows the line, after `: `, cut to [`MAX_FATAL_CHARS`] too,
/// without the indentation and the number with which the chain lists it, and without the code. A
/// line that is the forward's own error names the forward itself, and its chain holds no such
/// cause, so it stays alone. From an OpenVMM that predates the code, the cause that names the
/// forward by its text, [`failure::FORWARD_BIND_TEXT`], follows a line that does not name it, in
/// the same way.
fn read_fatal(path: &Path) -> Option<String> {
    let mut log = open_regular(path).ok()?;
    let start = last_line_start(&mut log, failure::FATAL_PREFIX.as_bytes()).ok()??;
    log.seek(SeekFrom::Start(start)).ok()?;
    let mut rest = Vec::new();
    log.read_to_end(&mut rest).ok()?;
    let text = String::from_utf8_lossy(&rest);
    let mut lines = text.lines();
    let mut fatal: String = lines.next()?.chars().take(MAX_FATAL_CHARS).collect();
    let tag = format!("[{}] ", failure::FORWARD_BIND_CODE);
    let coded = fatal
        .strip_prefix(failure::FATAL_PREFIX)
        .and_then(failure::leading_code)
        == Some(failure::FORWARD_BIND_CODE);
    let forward = if coded {
        lines
            .map(cause)
            .find_map(|named| named.strip_prefix(tag.as_str()))
    } else if fatal.contains(failure::FORWARD_BIND_TEXT) {
        None
    } else {
        lines
            .map(cause)
            .find(|named| named.starts_with(failure::FORWARD_BIND_TEXT))
    };
    if let Some(forward) = forward {
        fatal.push_str(": ");
        fatal.extend(forward.chars().take(MAX_FATAL_CHARS));
    }
    Some(fatal)
}

/// Returns the offset in `log` of its last line that starts with `prefix`, or none if no line
/// does, reading the log back from its end, [`FATAL_BLOCK_BYTES`] at a time: a line starts at
/// the start of the log, and after each newline.
fn last_line_start(log: &mut File, prefix: &[u8]) -> io::Result<Option<u64>> {
    let length = log.seek(SeekFrom::End(0))?;
    let mut block = Vec::new();
    let mut end = length;
    while end > 0 {
        let start = end.saturating_sub(FATAL_BLOCK_BYTES);
        // The block's read goes on past its end by the prefix's length, which a line that starts
        // at its end would begin with.
        let stop = length.min(end + prefix.len() as u64);
        block.resize((stop - start) as usize, 0);
        log.seek(SeekFrom::Start(start))?;
        log.read_exact(&mut block)?;
        let starts = block[..(end - start) as usize]
            .iter()
            .enumerate()
            .rev()
            .filter(|&(_, &byte)| byte == b'\n')
            .map(|(newline, _)| newline + 1)
            .chain((start == 0).then_some(0));
        for line in starts {
            if block[line..].starts_with(prefix) {
                return Ok(Some(start + line as u64));
            }
        }
        end = start;
    }
    Ok(None)
}

/// Returns the cause of an error's chain that `line` lists, as OpenVMM writes the chain after the
/// line that leads its fatal error, `    N: CAUSE` for each of several causes, or `    CAUSE` for
/// one alone: without its indentation, and its number, if it has one.
fn cause(line: &str) -> &str {
    let line = line.trim_start();
    match line.split_once(": ") {
        Some((number, cause))
            if !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) =>
        {
            cause
        }
        _ => line,
    }
}

/// What the sandbox of a clone keeps of it, as [`CloneRecords::read`] reads it: the lease that its
/// clone binding records, the store's records of its launch and of its VM, its progress, the
/// record of its failure, and OpenVMM's outcome report of its VM.
///
/// The records tell the state of a clone, as an operation that lists, inspects, or removes
/// clones reads it:
///
/// - a [`journal`](Self::journal) without a [`runtime`](Self::runtime) record: a launch that runs,
///   under the lock of the sandbox, or one that was interrupted, which
///   [`reattach_clone`](super::reattach_clone) ends;
/// - a runtime record: a VM that may run, which a reattach holds again, or a stop ends;
/// - a [`failure`](Self::failure): a clone that failed, whose teardown is unresolved while the
///   journal or the runtime record remains, and [pending](Teardown::Pending) if a crash of the
///   caller interrupted its teardown, which a reattach finishes;
/// - neither a journal nor a runtime record: no VM of the clone runs, and a release ends the
///   lease, which removes the clone's records, before a removal of the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CloneRecords {
    /// The lease that the sandbox's clone binding records.
    pub lease: Lease,
    /// The journal of the clone's launch, while it runs, or once it was interrupted.
    pub journal: Option<LaunchJournal>,
    /// The runtime record of the clone's VM, from the commit of its launch until a stop, a
    /// termination, or a reattach verifies that the VM exited.
    pub runtime: Option<RuntimeRecord>,
    /// How far the clone got, as its progress log and the store's records of its launch tell.
    pub progress: Progress,
    /// The record of the clone's failure, if it failed from its launch on.
    pub failure: Option<FailureRecord>,
    /// OpenVMM's outcome report of the clone's VM, once its VMM wrote one, and none before the
    /// clone got past its plan, as [`read`](Self::read) tells.
    pub report: Option<Report>,
}

impl CloneRecords {
    /// Reads the records of the clone of the sandbox of `sandbox`, or returns `None` if the
    /// sandbox holds no clone binding.
    ///
    /// It takes no lock, as an inspection of the sandbox does not: each record is read whole, as
    /// the latest transition that replaced it left it, and the records may come from transitions
    /// that ran between the reads. A caller that holds the sandbox's lock reads them at one point
    /// of its lifecycle. Fails as the reads of the clone binding, [`Lease::of`], of the store's
    /// records, of the progress log, and of the failure record fail, a failure record of another
    /// clone than the binding's included; an outcome report that is missing or malformed is none.
    ///
    /// It reads the outcome report only once the clone got past its plan, as the progress log of
    /// its launch, the launch's journal, the runtime record of its VM, or the record of a failure
    /// past the plan shows. Before the launch writes the first of those, its progress log, it
    /// removes the report that an earlier VM of the sandbox left, which no acquisition, clearing
    /// of a VM's runtime record, or release removes, and which is not the clone's.
    pub fn read(sandbox: &SandboxStore) -> Result<Option<Self>> {
        let Some(lease) = Lease::of(sandbox)? else {
            return Ok(None);
        };
        let journal = sandbox.journal()?;
        let runtime = sandbox.runtime()?;
        let logged = read_progress(&sandbox.clone_progress_path(), lease.launch())?
            .map(|logged| logged.progress);
        let failure = read_failure_of(&sandbox.clone_failure_path(), &lease)?;
        // A failure records how far the clone got, which the store's records of its launch no
        // longer tell once the failure ended the launch.
        let progress = progress_of(&lease, journal.as_ref(), runtime.as_ref(), logged).max(
            failure
                .as_ref()
                .map_or(Progress::Planned, |failure| failure.progress),
        );
        // The launch removed the report of an earlier VM of the sandbox before it wrote the first
        // of the clone's records, which a progress past the plan shows.
        let report = (progress > Progress::Planned)
            .then(|| report::read(&sandbox.outcome_path()))
            .flatten();
        Ok(Some(Self {
            lease,
            journal,
            runtime,
            progress,
            failure,
            report,
        }))
    }
}

#[cfg(test)]
mod tests;
