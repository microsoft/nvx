//! The failures of clones, [`CloneFailure`]: the [`Evidence`] of a failure of a clone's plan,
//! launch, restore sequence, or reattach, the [`Outcome`] to which the evidence attributes it, as
//! [the attribution of evidence](Evidence#attribution) describes, the [`Stage`] at which the clone
//! failed, the clone's [`Progress`] by then, and the [`Teardown`] that followed, which the
//! failure's [`FailureRecord`] keeps, and the clone's sandbox records, as [the records of
//! failures](FailureRecord#records) describe.

use std::fmt;
use std::io;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::activation::{TemplateReference, TransactionId};
use crate::error::{Error, ErrorCode, ErrorKind, Result};
use crate::host::report::Report;
use crate::host::store::{LaunchId, Recovery};
use crate::host::template::{Lease, LeaseId, Store, StoreId};
use crate::protocol::status;

/// What the host saw of a failure of a clone, which attributes the failure to an [`Outcome`].
///
/// # Attribution
///
/// The outcome of a failure follows from its evidence alone, as [`outcome`](Self::outcome) gives
/// it:
///
/// | Evidence | Outcome | Quarantine |
/// | --- | --- | --- |
/// | An authorization that the image's binding denies; a stale image binding; invalid targets, format, settings, Activate, or mappings; an image that the template can't serve; a scratch that isn't ext4 or isn't fresh; a root that does not assemble from them | [`Request`](Outcome::Request) | No |
/// | A supplied file with the wrong identity, length, or geometry | [`Request`](Outcome::Request), or the host's [`Staging`](Outcome::Staging) once the caller proves it | No |
/// | An endpoint or forward that cannot be bound; resource exhaustion; a destination that cannot restore the template, such as an incompatible CPU or TSC; another runtime; another failure of the host's own operations | [`Host`](Outcome::Host) | No |
/// | A lost status; a timeout; a reset; a forced kill; a broken protocol | [`Unknown`](Outcome::Unknown): the host, the protocol, or neither | No |
/// | A malformed or inconsistent generation; a diagnosed capture or repair defect attributable to it | [`Template`](Outcome::Template) | Yes |
/// | A repair status alone, 193, 194, and 195 included | [`Repair`](Outcome::Repair): the repair stage, whose provenance is still to establish | No |
///
/// Only a failure of the template quarantines the template's generation, through the generation's
/// tombstone, as [`CloneFailure::quarantine`] does: no other evidence shows that the generation,
/// rather than the request, the host, or the session, failed the clone, and a quarantine changes
/// nothing of the generation's payload or snapshot, which its other clones keep restoring.
///
/// The host sees the guest of a clone through its control session, and through the status with
/// which its VM ends, which OpenVMM's outcome report gives: a clone's restore discards the
/// guest's console, so no diagnostic tells where a status came from. [`status_evidence`]
/// attributes a status alone, as [`nvx::protocol::status`](crate::protocol::status) classes it:
///
/// - a status of the time ABI or of a clone's repair is evidence of the repair stage, at any point,
///   since the repair's deferred work may end a clone that is ready, and its provenance is still to
///   establish, since a workload may end its VM with any status;
/// - a status of a clone's activation is evidence of its class while no workload can have run,
///   before the start of the clone's sandbox was sent, and none after;
/// - no other status is evidence.
///
/// A status that is evidence prevails over what the host or the caller saw of the same end: the
/// loss of the session, the passing of a deadline, or the VMM's exit only show that the clone
/// ended, and a step that fails as the guest ends the clone may fail because it ended. It does not
/// prevail over evidence of the template, a check of the generation or a diagnosis that attributes
/// a defect to it, which shows the provenance that the status alone leaves to establish, and the
/// failure's record holds the status beside it.
///
/// The host sees the failures of OpenVMM itself through the line with which OpenVMM reports the
/// error that ended it, which the launch's log keeps, and which leads with the error's time ABI
/// code, a spelling that OpenVMM keeps stable for its callers. A VMM that exits before its
/// restore-ready event with a code that says that the host cannot present the template's CPU
/// profile, TSC, or LAPIC timer failed on an [incompatible host](Self::IncompatibleHost). Any
/// other code, such as one of the snapshot's time contract, keeps what the host saw, since it does
/// not tell a defect of the generation from one of this host's OpenVMM, and the failure's message
/// keeps the line. One error that has no code tells the host's fault by its text alone: a VMM
/// whose error's chain names a forward of the clone's network whose port of host loopback OpenVMM
/// cannot bind, as one that another socket holds, which OpenVMM binds as it restores the clone's
/// NIC, before its restore-ready event, failed to [bind a forward](Self::ForwardBind). The failure's
/// message keeps that cause of the chain after the line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Evidence {
    /// The verdict of the distro policy that the binding of the clone's image holds rejects the
    /// image: the request may not run it.
    AuthorizationDenied,
    /// The binding of the clone's image is stale: it holds a verdict of another revision of the
    /// distro policy than the caller accepts, or it is of another version than this build checks,
    /// or the image's manifest changed after it was made.
    StaleBinding,
    /// The request's targets are invalid: a memory target that is not whole blocks of the
    /// template's, from its RAM to its capacity, or resources that the repaired clone does not
    /// have, as the guest's
    /// [`ACTIVATION_RESOURCES`](crate::protocol::status::ACTIVATION_RESOURCES) tells.
    InvalidTargets,
    /// The request's image is not of the format and identity that its binding names.
    InvalidFormat,
    /// The request's settings are invalid: a plan that no clone takes, a path that OpenVMM would
    /// not open as checked, a network that the template's NIC does not match, an image or a
    /// scratch image that is a file of the sandbox's store, or settings that the guest cannot
    /// apply, as its [`ACTIVATION_WORKLOAD`](crate::protocol::status::ACTIVATION_WORKLOAD) and
    /// [`ACTIVATION_NETWORK`](crate::protocol::status::ACTIVATION_NETWORK) tell.
    InvalidSettings,
    /// The request's Activate is invalid: settings that no activation record holds, or a record
    /// that the guest refused, as its
    /// [`ACTIVATION_RECORD`](crate::protocol::status::ACTIVATION_RECORD) tells.
    InvalidActivate,
    /// The request's mappings are invalid: an export that no clone takes, such as one that
    /// overlaps the template store; an export whose read-only mappings the host cannot keep
    /// read-only, or whose objects are no longer those that its plan found, a missing one
    /// included; a mapping table that the guest refused, as the adapter's framing tells; or a
    /// host export that the guest cannot mount, as its
    /// [`ACTIVATION_EXPORT`](crate::protocol::status::ACTIVATION_EXPORT) tells.
    InvalidMappings,
    /// The request's image is one that the template cannot serve, as a diagnostic tells. No
    /// clone's restore keeps the guest's diagnostics yet, so the guest's status alone,
    /// [`UnusableRoot`](Self::UnusableRoot), tells of such an image.
    UnservableImage,
    /// The request's scratch image is not ext4, or not fresh: another launch holds it, or, as a
    /// diagnostic tells, the guest found a filesystem on it. No clone's restore keeps the guest's
    /// diagnostics yet, so the guest's status alone, [`UnusableRoot`](Self::UnusableRoot), tells
    /// of a filesystem on it.
    UnusableScratch,
    /// The clone's root does not assemble from the request's image over its upper, as the guest's
    /// [`ACTIVATION_ROOT`](crate::protocol::status::ACTIVATION_ROOT) tells: an image that the
    /// template cannot serve, or a scratch that is not ext4 or not fresh, which the status alone
    /// does not tell apart.
    UnusableRoot,
    /// A file that the request supplied is not the one that its binding names: it was replaced,
    /// changed, or removed after its binding was made, or its path names another file than the
    /// check found, or one that others may change.
    SuppliedIdentity,
    /// A file that the request supplied is of a length that the template's device cannot take: an
    /// image longer than the template's placeholder, or a scratch image of another length than the
    /// template's scratch.
    SuppliedLength,
    /// A file that the request supplied presents other block sizes than the template's device, or
    /// a geometry that OpenVMM refuses.
    SuppliedGeometry,
    /// An endpoint of the clone's launch cannot be bound: the endpoint of the VMM's restore-ready
    /// event, or a control socket whose path a Unix socket's address, or OpenVMM's option
    /// `listen=<path>`, cannot hold.
    EndpointBind,
    /// A forward of the clone's network cannot be bound: OpenVMM cannot bind its port of host
    /// loopback, as when another socket holds it, as it restores the clone's NIC, and exits before
    /// its restore-ready event, as [the attribution](Self#attribution) describes.
    ForwardBind,
    /// The host ran out of a resource, such as its storage, its memory, or its descriptors.
    ResourceExhaustion,
    /// The host cannot restore the template: this build speaks other contracts than the template,
    /// or reads no key of the template's encoding version, or OpenVMM refused to restore the
    /// snapshot with a time ABI code that says that the host cannot present the template's CPU
    /// profile, TSC, or LAPIC timer, as [the attribution](Self#attribution) describes.
    IncompatibleHost,
    /// The clone runs, or would launch, under another runtime identity than the template's or the
    /// caller's, which the clone's guest might not speak.
    RuntimeMismatch,
    /// Another failure of the host's own operations: an I/O error of its files, a store whose
    /// records are inconsistent, a clock that cannot hold a deadline, or a caller that drives the
    /// clone through another lease or lock than the clone's, or launches it again once it failed.
    HostFailure,
    /// The clone's status was lost: its VMM ended without an outcome report that tells why, as one
    /// that is killed does, or the caller of its launch crashed.
    LostStatus,
    /// A deadline of the clone's restore passed.
    Timeout,
    /// The adapter lost the clone's session: its reset, the end of its stream, or a failure of its
    /// transport.
    Reset,
    /// The host's recovery terminated the clone's VMM, whose launch's caller had crashed.
    ForcedKill,
    /// The clone's session broke its protocol: another process at one of the clone's endpoints,
    /// bytes that differ from OpenVMM's restore-ready event, a refusal of the adapter's framing, a
    /// Ready that does not report the record's activation, or a session that broke the order of
    /// the activation, as the guest's
    /// [`ACTIVATION_PROTOCOL`](crate::protocol::status::ACTIVATION_PROTOCOL) and
    /// [`ACTIVATION_ORDER`](crate::protocol::status::ACTIVATION_ORDER) tell.
    Protocol,
    /// The leased generation is malformed or inconsistent: the store indexes it with another key
    /// than the lease names, its key or snapshot is malformed, or its payload is not a directory
    /// of the current user with mode 0700.
    MalformedGeneration,
    /// A defect of the generation's capture or repair, which a diagnostic attributes to the
    /// generation. No clone's restore keeps the guest's diagnostics yet, so a status of the repair
    /// alone is [`RepairStatus`](Self::RepairStatus), and no time ABI code of OpenVMM's tells a
    /// defect of the generation from one of the host's OpenVMM.
    TemplateDefect,
    /// A status of the time ABI or of a clone's repair, alone, which
    /// [`FailureRecord::status`] holds.
    RepairStatus,
}

impl Evidence {
    /// Returns the outcome to which the evidence attributes a failure, as [the
    /// attribution](Self#attribution) describes.
    pub const fn outcome(self) -> Outcome {
        match self {
            Self::AuthorizationDenied
            | Self::StaleBinding
            | Self::InvalidTargets
            | Self::InvalidFormat
            | Self::InvalidSettings
            | Self::InvalidActivate
            | Self::InvalidMappings
            | Self::UnservableImage
            | Self::UnusableScratch
            | Self::UnusableRoot
            | Self::SuppliedIdentity
            | Self::SuppliedLength
            | Self::SuppliedGeometry => Outcome::Request,
            Self::EndpointBind
            | Self::ForwardBind
            | Self::ResourceExhaustion
            | Self::IncompatibleHost
            | Self::RuntimeMismatch
            | Self::HostFailure => Outcome::Host,
            Self::LostStatus | Self::Timeout | Self::Reset | Self::ForcedKill | Self::Protocol => {
                Outcome::Unknown
            }
            Self::MalformedGeneration | Self::TemplateDefect => Outcome::Template,
            Self::RepairStatus => Outcome::Repair,
        }
    }

    /// Returns whether the evidence is of a file that the request supplied, which the host's
    /// staging may have made wrong, as [`CloneFailure::with_staging_proven`] attributes it once
    /// the caller proves it.
    pub const fn is_supplied_file(self) -> bool {
        matches!(
            self,
            Self::SuppliedIdentity | Self::SuppliedLength | Self::SuppliedGeometry
        )
    }

    /// Returns the evidence's name, in lowercase words joined by hyphens, as its record spells it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AuthorizationDenied => "authorization-denied",
            Self::StaleBinding => "stale-binding",
            Self::InvalidTargets => "invalid-targets",
            Self::InvalidFormat => "invalid-format",
            Self::InvalidSettings => "invalid-settings",
            Self::InvalidActivate => "invalid-activate",
            Self::InvalidMappings => "invalid-mappings",
            Self::UnservableImage => "unservable-image",
            Self::UnusableScratch => "unusable-scratch",
            Self::UnusableRoot => "unusable-root",
            Self::SuppliedIdentity => "supplied-identity",
            Self::SuppliedLength => "supplied-length",
            Self::SuppliedGeometry => "supplied-geometry",
            Self::EndpointBind => "endpoint-bind",
            Self::ForwardBind => "forward-bind",
            Self::ResourceExhaustion => "resource-exhaustion",
            Self::IncompatibleHost => "incompatible-host",
            Self::RuntimeMismatch => "runtime-mismatch",
            Self::HostFailure => "host-failure",
            Self::LostStatus => "lost-status",
            Self::Timeout => "timeout",
            Self::Reset => "reset",
            Self::ForcedKill => "forced-kill",
            Self::Protocol => "protocol",
            Self::MalformedGeneration => "malformed-generation",
            Self::TemplateDefect => "template-defect",
            Self::RepairStatus => "repair-status",
        }
    }
}

impl fmt::Display for Evidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What a failure of a clone is attributed to, by its [`Evidence`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Outcome {
    /// The request: its image, upper, targets, or settings.
    Request,
    /// The host's staging of a file that the request supplied, which the caller proved.
    Staging,
    /// The host.
    Host,
    /// The host, the protocol, or neither: the evidence shows no provenance.
    Unknown,
    /// The leased generation of the template, which [`CloneFailure::quarantine`] quarantines.
    Template,
    /// The repair stage of the clone, whose provenance is still to establish.
    Repair,
}

impl Outcome {
    /// Returns whether a failure of this outcome quarantines the leased generation, through its
    /// tombstone: only a failure of the [template](Self::Template) does.
    pub const fn quarantines(self) -> bool {
        matches!(self, Self::Template)
    }

    /// Returns the outcome's name, in lowercase, as its record spells it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Staging => "staging",
            Self::Host => "host",
            Self::Unknown => "unknown",
            Self::Template => "template",
            Self::Repair => "repair",
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The stage of a clone at which it failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Stage {
    /// The clone's plan, [`ClonePlan::new`](super::ClonePlan::new), before anything of its launch.
    Plan,
    /// The clone's launch, [`ClonePlan::launch`](super::ClonePlan::launch), until its VMM runs.
    Launch,
    /// OpenVMM's restore, from the spawn of the VMM until its restore-ready event.
    Restore,
    /// The clone's repair and activation, from the restore-ready event until the check of the
    /// clone's Ready.
    Activation,
    /// The start of the clone's sandbox, from the check of its Ready, through the mapping table of
    /// a clone with an export, until the start's confirmation.
    Start,
    /// The commit of the clone's launch.
    Commit,
    /// The run of the clone's VM, from the commit of its launch until the VM exits.
    Run,
    /// A reattach's check of the clone, [`reattach_clone`](super::reattach_clone).
    Reattach,
}

impl Stage {
    /// Returns the stage's name, in lowercase, as its record spells it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Launch => "launch",
            Self::Restore => "restore",
            Self::Activation => "activation",
            Self::Start => "start",
            Self::Commit => "commit",
            Self::Run => "run",
            Self::Reattach => "reattach",
        }
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// How far a clone got, in the order of its steps: the latest step that it reached.
///
/// The steps from the spawn of the clone's VMM to the start's confirmation are those that its
/// launch and restore sequence record in its progress log,
/// [`CLONE_PROGRESS_NAME`](crate::host::store::CLONE_PROGRESS_NAME), as each is reached; the
/// store's records tell the spawn too, and the commit: the launch's journal records the spawn of
/// its VMM until the launch ends, and the runtime record its commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Progress {
    /// The clone's plan: nothing of its launch exists.
    Planned,
    /// The launch wrote the clone's files, and may have begun its journal: no VMM was spawned.
    Launching,
    /// The launch's VMM was spawned, which its journal records, unless the launch could not record
    /// it, and then ended it through its handle.
    Spawned,
    /// The VMM's restore-ready event was read: the restored VM runs.
    Event,
    /// A session of the adapter's framing attached: the guest's repair acknowledged the restore,
    /// before which OpenVMM reads none of the host's input.
    Acknowledged,
    /// The Activate of the clone's record was sent.
    Activating,
    /// The clone's Ready was checked: the clone is activated, with the record's transaction. A
    /// clone with an export gets its mapping table at this step, before the start.
    Ready,
    /// The start of the clone's sandbox was sent, once the guest applied the whole mapping table
    /// of a clone with an export.
    Starting,
    /// The start was confirmed: the clone's sandbox started.
    Started,
    /// The launch committed: the runtime record names the clone's VM.
    Committed,
}

impl Progress {
    /// Returns the stage of a clone that fails once it got this far: the stage of the step that
    /// follows.
    pub const fn stage(self) -> Stage {
        match self {
            Self::Planned => Stage::Plan,
            Self::Launching => Stage::Launch,
            Self::Spawned => Stage::Restore,
            Self::Event | Self::Acknowledged | Self::Activating => Stage::Activation,
            Self::Ready | Self::Starting => Stage::Start,
            Self::Started => Stage::Commit,
            Self::Committed => Stage::Run,
        }
    }

    /// Returns the step's name, in lowercase, as its records spell it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Launching => "launching",
            Self::Spawned => "spawned",
            Self::Event => "event",
            Self::Acknowledged => "acknowledged",
            Self::Activating => "activating",
            Self::Ready => "ready",
            Self::Starting => "starting",
            Self::Started => "started",
            Self::Committed => "committed",
        }
    }

    /// Returns whether a workload may have run in the clone once it got this far: once the start
    /// of its sandbox was sent.
    pub const fn may_have_run(self) -> bool {
        matches!(self, Self::Starting | Self::Started | Self::Committed)
    }
}

impl fmt::Display for Progress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// How the teardown that follows a failure of a clone ended.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Teardown {
    /// The clone failed before its VMM was spawned: no VM runs.
    NoVm,
    /// No VMM of the clone runs: the VMM that the launch spawned has exited, as its verified
    /// handle proved, or a launch's claim on its log, which no process holds, proved for the
    /// launch, whether it spawned a VMM or not. The store's records of the VMM are cleared.
    Exited,
    /// The teardown terminated the clone's VMM through its verified identity, or through its
    /// handle, a VMM that the launch could not record, and confirmed its exit, and the store's
    /// records of it are cleared.
    Terminated,
    /// Whether the clone's VMM still runs cannot be determined, for the reason that this gives:
    /// its identity or its exit is indeterminate. The launch's journal or the runtime record
    /// remains, and with it the clone's lease, which a release refuses to end until a reattach
    /// resolves the teardown. A record that the clone's sandbox keeps holds at most 1024
    /// characters of the reason, as it holds at most 8192 of the
    /// [message](FailureRecord::message).
    Unresolved(String),
    /// The teardown had not ended when the failure was recorded: the record that a failure writes
    /// before its teardown, so that a crash of the caller during the teardown leaves it, which the
    /// record that the failure writes once its teardown ended replaces. A reattach that finds it
    /// finishes the failure, as [`reattach_clone`](super::reattach_clone) describes, so no
    /// [`CloneFailure`] that a step of a clone returns has it.
    Pending,
}

impl Teardown {
    /// Returns whether the teardown is resolved: whether no VM of the clone runs.
    pub fn is_resolved(&self) -> bool {
        !matches!(self, Self::Unresolved(_) | Self::Pending)
    }

    /// Returns the teardown of a launch that [`Starting::abandon`](crate::host::supervisor::Starting::abandon),
    /// or a recovery, ended as `ended` tells.
    pub(super) fn of(ended: Result<Recovery>) -> Self {
        match ended {
            Ok(Recovery::Clean | Recovery::Exited) => Self::Exited,
            Ok(Recovery::Terminated(_)) => Self::Terminated,
            Ok(Recovery::Indeterminate(reason)) => Self::Unresolved(reason),
            Err(error) => Self::Unresolved(error.to_string()),
        }
    }

    /// Returns the teardown's name, in lowercase words joined by hyphens, as its record spells it.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::NoVm => "no-vm",
            Self::Exited => "exited",
            Self::Terminated => "terminated",
            Self::Unresolved(_) => "unresolved",
            Self::Pending => "pending",
        }
    }
}

/// The record of a failure of a clone: what the failure was about, where and how it failed, and
/// how its teardown ended, as [`CloneFailure::record`] returns it.
///
/// # Records
///
/// A failure from the clone's launch on is recorded in the clone's sandbox, as
/// [`CLONE_FAILURE_NAME`](crate::host::store::CLONE_FAILURE_NAME): before the teardown that
/// follows it, whose record is [pending](Teardown::Pending) until then, so that a crash of the
/// caller during the teardown leaves the record, which a reattach finishes, and once its teardown
/// is known, so that the record holds the stage, the evidence and the status, the original
/// outcome, the transaction of the clone's restore, the leased generation, the clone's progress,
/// and the teardown's outcome. A failure that cannot be recorded before its teardown removes
/// nothing that tells that the clone launched: the journal of its launch, or the runtime record of
/// its VM, remains, and with it the lease, so that a reattach still finds that the clone failed,
/// though not how, if a crash of the caller comes before the record. A failure of the plan, before
/// the launch, leaves no VM and no record: the caller holds the failure, and releases the lease. A
/// launch keeps its lease, and
/// with it the leased generation, until its VM has exited, so a failure that comes after the
/// clone's Ready, such as one of the repair's deferred work, is classified against that
/// generation all the same, and so is the status with which the guest ended a VM whose exit a
/// reattach confirms only later.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct FailureRecord {
    /// The identity of the template store that leased the clone's generation.
    pub store: StoreId,
    /// The identity of the clone's lease.
    pub lease: LeaseId,
    /// The leased generation, and the identity of its key.
    pub template: TemplateReference,
    /// The launch that the lease reserves.
    pub launch: LaunchId,
    /// The transaction ID of the clone's restore, once its plan drew it.
    pub transaction: Option<TransactionId>,
    /// The stage at which the clone failed.
    pub stage: Stage,
    /// How far the clone got: its acknowledgment and the progress of its activation included.
    pub progress: Progress,
    /// The evidence of the failure.
    pub evidence: Evidence,
    /// The status with which the guest ended the clone's VM, as OpenVMM's outcome report gives
    /// it, if the report has one.
    pub status: Option<u8>,
    /// The original outcome of the failure, to which its evidence attributes it, whatever the
    /// teardown that followed it, and the status with which the guest ended the VM, once the VMM
    /// exited.
    pub outcome: Outcome,
    /// How the teardown that followed the failure ended.
    pub teardown: Teardown,
    /// The code of the failure's error.
    pub code: ErrorCode,
    /// The message of the failure's error, followed by the messages of its sources. A record that
    /// the clone's sandbox keeps holds at most 8192 of its characters: of a longer message, as a
    /// caller's diagnosis may be, its first and its last ones, with an ellipsis between them.
    pub message: String,
}

impl FailureRecord {
    /// Returns the record of a failure of the clone that holds `lease`, with `transaction`, at
    /// the stage and the progress `at`, with `evidence`, the guest's `status`, and `teardown`,
    /// whose error is `error`.
    pub(super) fn new(
        lease: &Lease,
        transaction: Option<TransactionId>,
        (stage, progress): (Stage, Progress),
        evidence: Evidence,
        status: Option<u8>,
        teardown: Teardown,
        error: &Error,
    ) -> Self {
        Self {
            store: lease.store(),
            lease: lease.id(),
            template: lease.template(),
            launch: lease.launch().clone(),
            transaction,
            stage,
            progress,
            evidence,
            status,
            outcome: evidence.outcome(),
            teardown,
            code: error.code(),
            message: describe(error),
        }
    }

    /// Returns the error that the record describes, with its code and its message.
    pub fn error(&self) -> Error {
        Error::new(self.code, self.message.clone())
    }
}

/// A failure of a clone: the error of the step that failed, with the [`FailureRecord`] that
/// attributes it, as the clone's plan, launch, restore sequence, and reattach return it.
///
/// It displays as its error, whose [`kind`](Self::kind) and [`code`](Self::code) it has, and
/// converts into it, without the record. A failure from the clone's launch on is recorded in the
/// clone's sandbox as well, as [the records of failures](FailureRecord#records) describe.
#[derive(Debug)]
pub struct CloneFailure {
    /// The error of the step that failed.
    error: Error,
    /// The failure's record.
    record: Box<FailureRecord>,
}

impl CloneFailure {
    /// Returns the failure of `error`, which `record` attributes.
    pub(super) fn new(error: Error, record: FailureRecord) -> Self {
        Self {
            error,
            record: Box::new(record),
        }
    }

    /// Returns the error of the step that failed.
    pub fn error(&self) -> &Error {
        &self.error
    }

    /// Returns the error of the step that failed, without the record.
    pub fn into_error(self) -> Error {
        self.error
    }

    /// Returns the error's kind.
    pub fn kind(&self) -> ErrorKind {
        self.error.kind()
    }

    /// Returns the error's code.
    pub fn code(&self) -> ErrorCode {
        self.error.code()
    }

    /// Returns the error's message.
    pub fn message(&self) -> &str {
        self.error.message()
    }

    /// Returns the failure's record.
    pub fn record(&self) -> &FailureRecord {
        &self.record
    }

    /// Returns the outcome to which the failure is attributed.
    pub fn outcome(&self) -> Outcome {
        self.record.outcome
    }

    /// Returns how the teardown that followed the failure ended.
    pub fn teardown(&self) -> &Teardown {
        &self.record.teardown
    }

    /// Returns the failure attributed to the host's staging, once the caller proved that its
    /// staging, rather than the request, made a file that the request supplied wrong: a failure
    /// whose evidence is a [supplied file](Evidence::is_supplied_file) with the wrong identity,
    /// length, or geometry, as the plan finds it, which records nothing. Any other failure is
    /// returned unchanged.
    pub fn with_staging_proven(mut self) -> Self {
        if self.record.evidence.is_supplied_file() {
            self.record.outcome = Outcome::Staging;
        }
        self
    }

    /// Quarantines the leased generation through its tombstone, as
    /// [`Store::tombstone`] does, waiting for the store's lock until `deadline`, or for as long as
    /// it takes without one, if the failure is the [template's](Outcome::Template), and returns
    /// whether it did. A failure of any other outcome quarantines nothing.
    ///
    /// The tombstone changes nothing of the generation's payload or snapshot: the generation takes
    /// no new lease, while the clones that lease it keep running, and a retirement deletes it once
    /// they have released it. The store's lock comes before a sandbox's in the
    /// [lock order](crate::host::supervisor#the-lock-order), so the caller quarantines once it
    /// released the lock of the clone's sandbox, such as the one that an
    /// [`Acquired`](crate::host::template::Acquired) holds.
    ///
    /// Fails with [`ErrorKind::Other`] for a store of another identity than the one that leased
    /// the generation, and as [`Store::tombstone`] fails.
    pub fn quarantine(&self, store: &Store, deadline: Option<Instant>) -> Result<bool> {
        if !self.record.outcome.quarantines() {
            return Ok(false);
        }
        if store.id() != self.record.store {
            return Err(Error::other(format!(
                "the generation {} that a failure quarantines is one of the template store {}, \
                 not of this one, {}",
                self.record.template.generation,
                self.record.store,
                store.id()
            )));
        }
        store.tombstone(self.record.template.generation, deadline)?;
        Ok(true)
    }
}

impl fmt::Display for CloneFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.error, formatter)
    }
}

impl std::error::Error for CloneFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        std::error::Error::source(&self.error)
    }
}

impl From<CloneFailure> for Error {
    fn from(failure: CloneFailure) -> Self {
        failure.error
    }
}

/// The error of a step of a clone, with the evidence that attributes it, before the stage at which
/// the clone failed, its progress, and the teardown that follows are known.
#[derive(Debug)]
pub(super) struct Fault {
    /// The evidence of the failure.
    pub(super) evidence: Evidence,
    /// The step's error.
    pub(super) error: Error,
}

impl Fault {
    /// Returns the fault of `error`, which `evidence` attributes.
    pub(super) fn new(evidence: Evidence, error: Error) -> Self {
        Self { evidence, error }
    }

    /// Returns the fault of `error`, a failure of the host's own operations, as
    /// [`host_evidence`] attributes it.
    pub(super) fn host(error: Error) -> Self {
        Self::new(host_evidence(&error), error)
    }
}

/// Attributes the error of a step of a clone.
pub(super) trait Attribute<T> {
    /// Returns the result with its error attributed by `evidence`.
    fn attribute(self, evidence: Evidence) -> std::result::Result<T, Fault>;

    /// Returns the result with its error attributed as a failure of the host's own operations, as
    /// [`host_evidence`] attributes it.
    fn on_host(self) -> std::result::Result<T, Fault>;
}

impl<T> Attribute<T> for Result<T> {
    fn attribute(self, evidence: Evidence) -> std::result::Result<T, Fault> {
        self.map_err(|error| Fault::new(evidence, error))
    }

    fn on_host(self) -> std::result::Result<T, Fault> {
        self.map_err(Fault::host)
    }
}

/// Returns the evidence of `error`, a failure of the host's own operations: resource exhaustion
/// if an I/O error among its sources says that the host ran out of storage, memory, or
/// descriptors, and a host failure otherwise.
pub(super) fn host_evidence(error: &Error) -> Evidence {
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        if let Some(io) = cause.downcast_ref::<io::Error>()
            && exhausted(io)
        {
            return Evidence::ResourceExhaustion;
        }
        source = cause.source();
    }
    Evidence::HostFailure
}

/// Returns whether `error` says that the host ran out of a resource.
fn exhausted(error: &io::Error) -> bool {
    // Too many open files, in the system or in the process.
    #[cfg(unix)]
    if [nix::errno::Errno::ENFILE, nix::errno::Errno::EMFILE]
        .into_iter()
        .any(|errno| error.raw_os_error() == Some(errno as i32))
    {
        return true;
    }
    matches!(
        error.kind(),
        io::ErrorKind::StorageFull
            | io::ErrorKind::QuotaExceeded
            | io::ErrorKind::FileTooLarge
            | io::ErrorKind::OutOfMemory
    )
}

/// Returns the message of `error`, followed by the messages of its sources, as the record of a
/// failure keeps it.
pub(super) fn describe(error: &Error) -> String {
    let mut text = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// Returns `error` with `addition` after its message, with its code and its source.
pub(super) fn amended(error: Error, addition: &str) -> Error {
    let code = error.code();
    let (_, message, source) = error.into_parts();
    let error = Error::new(code, format!("{message}{addition}"));
    match source {
        Some(source) => error.with_source(source),
        None => error,
    }
}

/// Returns the evidence of `status`, with which the guest ended a clone's VM once the clone got
/// as far as `progress`, if the status alone is evidence, as [the attribution of
/// evidence](Evidence#attribution) describes: a status of the time ABI or of a clone's repair is
/// evidence of the repair stage at any point, a status of a clone's activation is evidence of its
/// class before the start of the clone's sandbox was sent, and no other status is evidence.
pub fn status_evidence(status: u8, progress: Progress) -> Option<Evidence> {
    if status::TIME_STATUSES.contains(&status) || status::REPAIR_STATUSES.contains(&status) {
        return Some(Evidence::RepairStatus);
    }
    if progress.may_have_run() {
        return None;
    }
    Some(match status {
        status::ACTIVATION_DEADLINE => Evidence::Timeout,
        status::ACTIVATION_PROTOCOL | status::ACTIVATION_ORDER => Evidence::Protocol,
        status::ACTIVATION_RECORD => Evidence::InvalidActivate,
        status::ACTIVATION_RESOURCES => Evidence::InvalidTargets,
        status::ACTIVATION_ROOT => Evidence::UnusableRoot,
        status::ACTIVATION_WORKLOAD | status::ACTIVATION_NETWORK => Evidence::InvalidSettings,
        status::ACTIVATION_EXPORT => Evidence::InvalidMappings,
        _ => return None,
    })
}

/// Returns the guest's status in OpenVMM's outcome report `report` of a clone's VM, and the
/// evidence that the status alone is once the clone got as far as `progress`, as
/// [`status_evidence`] gives it: the status of a report whose category is `guest-exit` or
/// `success`, the guest's own end, and none for any other, such as a failure of the VMM.
pub(super) fn reported(
    report: Option<&Report>,
    progress: Progress,
) -> (Option<Evidence>, Option<u8>) {
    let Some(report) = report else {
        return (None, None);
    };
    let status = match (report.category.as_deref(), report.status_code) {
        (Some("guest-exit" | "success"), Some(code)) => u8::try_from(code).ok(),
        _ => None,
    };
    let evidence = status.and_then(|status| status_evidence(status, progress));
    (evidence, status)
}

/// Returns the evidence of a failure whose step saw `evidence`, once the guest's status, which
/// ended the clone's VM, is `reported`, as [`reported`] gives it, as [the attribution of
/// evidence](Evidence#attribution) describes: the status's evidence prevails, unless `evidence`
/// is of the template, whose provenance the status alone does not show.
pub(super) fn prevailing(evidence: Evidence, reported: Option<Evidence>) -> Evidence {
    match reported {
        Some(reported) if evidence.outcome() != Outcome::Template => reported,
        _ => evidence,
    }
}

/// The start of the line with which OpenVMM reports, on its standard error, which the launch's log
/// keeps, the error that ended it: the line goes on with the error's first time ABI code in
/// brackets, such as `[E_TSC_RATE_TOLERANCE]`, if the error's chain has one.
pub(super) const FATAL_PREFIX: &str = "fatal error: ";

/// The time ABI codes with which OpenVMM refuses to restore a snapshot on a host that cannot
/// present the template's CPU or time: a CPU that the template's CPU profile does not serve, a
/// hypervisor that cannot present the profile, as OpenVMM groups the failures of a profile, and a
/// TSC or a LAPIC timer that the snapshot's time contract does not accept.
const INCOMPATIBLE_HOST_CODES: [&str; 13] = [
    "E_PROFILE_HOST_UNKNOWN",
    "E_CPU_GENERATION",
    "E_PROFILE_UNSUPPORTED",
    "E_PROFILE_TIME_BITS",
    "E_CPU_SURFACE",
    "E_CPU_UNLISTED",
    "E_IDENTITY_ROUTING",
    "E_TSC_SYNC_UNSUPPORTED",
    "E_TSC_SCALING_ACTIVE",
    "E_TSC_RATE_UNAVAILABLE",
    "E_TSC_RATE_TOLERANCE",
    "E_LAPIC_RATE_UNAVAILABLE",
    "E_LAPIC_RATE_MISMATCH",
];

/// The text with which a cause of OpenVMM's fatal error names a forward of a clone's network whose
/// port of host loopback OpenVMM cannot bind as it restores the clone's NIC: the forward's
/// protocol, `Tcp` or `Udp`, its address, and its port follow, then `)`, as the excerpt of
/// OpenVMM's source `tests/vectors/openvmm/port_forward_socket_creation.rs` spells the error.
/// OpenVMM gives the error no code.
pub(super) const FORWARD_BIND_TEXT: &str = "failed to create socket for port forward (";

/// Returns the evidence of `line`, the line with which OpenVMM reported the error that ended a
/// clone's VMM before its restore-ready event, followed by the cause of the error's chain that
/// names a forward that OpenVMM cannot bind, if it has one, as the launch reads them from the end
/// of the VMM's log, as [the attribution of evidence](Evidence#attribution) describes:
/// [`Evidence::IncompatibleHost`] for an error whose time ABI code says that the host cannot
/// present the template's CPU or time, [`Evidence::ForwardBind`] for one that names a forward that
/// OpenVMM cannot bind, [`FORWARD_BIND_TEXT`], and none for any other.
pub(super) fn fatal_evidence(line: &str) -> Option<Evidence> {
    let reported = line.strip_prefix(FATAL_PREFIX)?;
    let code = reported
        .strip_prefix('[')
        .and_then(|code| code.get(..code.find(']')?));
    if code.is_some_and(|code| INCOMPATIBLE_HOST_CODES.contains(&code)) {
        return Some(Evidence::IncompatibleHost);
    }
    reported
        .contains(FORWARD_BIND_TEXT)
        .then_some(Evidence::ForwardBind)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every evidence, in the order of the module's table.
    const ALL: [Evidence; 27] = [
        Evidence::AuthorizationDenied,
        Evidence::StaleBinding,
        Evidence::InvalidTargets,
        Evidence::InvalidFormat,
        Evidence::InvalidSettings,
        Evidence::InvalidActivate,
        Evidence::InvalidMappings,
        Evidence::UnservableImage,
        Evidence::UnusableScratch,
        Evidence::UnusableRoot,
        Evidence::SuppliedIdentity,
        Evidence::SuppliedLength,
        Evidence::SuppliedGeometry,
        Evidence::EndpointBind,
        Evidence::ForwardBind,
        Evidence::ResourceExhaustion,
        Evidence::IncompatibleHost,
        Evidence::RuntimeMismatch,
        Evidence::HostFailure,
        Evidence::LostStatus,
        Evidence::Timeout,
        Evidence::Reset,
        Evidence::ForcedKill,
        Evidence::Protocol,
        Evidence::MalformedGeneration,
        Evidence::TemplateDefect,
        Evidence::RepairStatus,
    ];

    #[test]
    fn each_row_of_the_table_attributes_its_evidence() {
        let rows: [(&[Evidence], Outcome); 6] = [
            (
                &[
                    Evidence::AuthorizationDenied,
                    Evidence::StaleBinding,
                    Evidence::InvalidTargets,
                    Evidence::InvalidFormat,
                    Evidence::InvalidSettings,
                    Evidence::InvalidActivate,
                    Evidence::InvalidMappings,
                    Evidence::UnservableImage,
                    Evidence::UnusableScratch,
                    Evidence::UnusableRoot,
                ],
                Outcome::Request,
            ),
            (
                &[
                    Evidence::SuppliedIdentity,
                    Evidence::SuppliedLength,
                    Evidence::SuppliedGeometry,
                ],
                Outcome::Request,
            ),
            (
                &[
                    Evidence::EndpointBind,
                    Evidence::ForwardBind,
                    Evidence::ResourceExhaustion,
                    Evidence::IncompatibleHost,
                    Evidence::RuntimeMismatch,
                    Evidence::HostFailure,
                ],
                Outcome::Host,
            ),
            (
                &[
                    Evidence::LostStatus,
                    Evidence::Timeout,
                    Evidence::Reset,
                    Evidence::ForcedKill,
                    Evidence::Protocol,
                ],
                Outcome::Unknown,
            ),
            (
                &[Evidence::MalformedGeneration, Evidence::TemplateDefect],
                Outcome::Template,
            ),
            (&[Evidence::RepairStatus], Outcome::Repair),
        ];
        let mut seen = Vec::new();
        for (evidence, outcome) in rows {
            for evidence in evidence {
                assert_eq!(evidence.outcome(), outcome, "{evidence}");
                assert_eq!(
                    evidence.outcome().quarantines(),
                    outcome == Outcome::Template,
                    "{evidence}"
                );
                assert_eq!(
                    evidence.is_supplied_file(),
                    evidence.as_str().starts_with("supplied-"),
                    "{evidence}"
                );
                seen.push(*evidence);
            }
        }
        // The table attributes every evidence once.
        assert_eq!(seen, ALL);
        // Only the template's outcome quarantines.
        for outcome in [
            Outcome::Request,
            Outcome::Staging,
            Outcome::Host,
            Outcome::Unknown,
            Outcome::Repair,
        ] {
            assert!(!outcome.quarantines(), "{outcome}");
        }
        assert!(Outcome::Template.quarantines());
    }

    #[test]
    fn names_are_those_of_the_records() {
        for evidence in ALL {
            let json = serde_json::to_string(&evidence).unwrap();
            assert_eq!(json, format!("\"{evidence}\""));
            assert_eq!(serde_json::from_str::<Evidence>(&json).unwrap(), evidence);
        }
        let progress = [
            Progress::Planned,
            Progress::Launching,
            Progress::Spawned,
            Progress::Event,
            Progress::Acknowledged,
            Progress::Activating,
            Progress::Ready,
            Progress::Starting,
            Progress::Started,
            Progress::Committed,
        ];
        // The steps are in order, and spelled as their records spell them.
        assert!(progress.windows(2).all(|pair| pair[0] < pair[1]));
        for step in progress {
            assert_eq!(serde_json::to_string(&step).unwrap(), format!("\"{step}\""));
        }
        let stages: Vec<Stage> = progress.iter().map(|step| step.stage()).collect();
        assert_eq!(
            stages,
            [
                Stage::Plan,
                Stage::Launch,
                Stage::Restore,
                Stage::Activation,
                Stage::Activation,
                Stage::Activation,
                Stage::Start,
                Stage::Start,
                Stage::Commit,
                Stage::Run,
            ]
        );
        for stage in [
            Stage::Plan,
            Stage::Launch,
            Stage::Restore,
            Stage::Activation,
            Stage::Start,
            Stage::Commit,
            Stage::Run,
            Stage::Reattach,
        ] {
            assert_eq!(
                serde_json::to_string(&stage).unwrap(),
                format!("\"{stage}\"")
            );
        }
        for outcome in [
            Outcome::Request,
            Outcome::Staging,
            Outcome::Host,
            Outcome::Unknown,
            Outcome::Template,
            Outcome::Repair,
        ] {
            assert_eq!(
                serde_json::to_string(&outcome).unwrap(),
                format!("\"{outcome}\"")
            );
        }
    }

    #[test]
    fn a_repair_status_alone_is_evidence_of_the_repair_stage_at_any_point() {
        for status in status::TIME_STATUSES.chain(status::REPAIR_STATUSES) {
            for progress in [Progress::Spawned, Progress::Ready, Progress::Committed] {
                assert_eq!(
                    status_evidence(status, progress),
                    Some(Evidence::RepairStatus),
                    "{status} {progress}"
                );
            }
        }
    }

    #[test]
    fn an_activation_status_is_evidence_of_its_class_until_a_workload_may_have_run() {
        let classes = [
            (status::ACTIVATION_DEADLINE, Evidence::Timeout),
            (status::ACTIVATION_PROTOCOL, Evidence::Protocol),
            (status::ACTIVATION_RECORD, Evidence::InvalidActivate),
            (status::ACTIVATION_RESOURCES, Evidence::InvalidTargets),
            (status::ACTIVATION_ROOT, Evidence::UnusableRoot),
            (status::ACTIVATION_WORKLOAD, Evidence::InvalidSettings),
            (status::ACTIVATION_EXPORT, Evidence::InvalidMappings),
            (status::ACTIVATION_NETWORK, Evidence::InvalidSettings),
            (status::ACTIVATION_ORDER, Evidence::Protocol),
        ];
        assert_eq!(
            classes.map(|(status, _)| status).to_vec(),
            status::ACTIVATION_STATUSES.collect::<Vec<_>>()
        );
        for (status, evidence) in classes {
            for progress in [Progress::Spawned, Progress::Activating, Progress::Ready] {
                assert_eq!(
                    status_evidence(status, progress),
                    Some(evidence),
                    "{status} {progress}"
                );
            }
            // Once the start was sent, a workload may end the VM with any status.
            for progress in [Progress::Starting, Progress::Started, Progress::Committed] {
                assert_eq!(
                    status_evidence(status, progress),
                    None,
                    "{status} {progress}"
                );
            }
        }
        // No other status is evidence: a success, the generic failure, or a workload's.
        for status in [0, 1, 2, 3, 39, 46, 49, 59, 137, 192, 196, 255] {
            assert_eq!(
                status_evidence(status, Progress::Activating),
                None,
                "{status}"
            );
        }
    }

    #[test]
    fn a_report_gives_the_guest_s_status_and_its_evidence() {
        let report = |category: &str, status_code: i64| Report {
            category: Some(category.to_owned()),
            status_code: Some(status_code),
            teardown_complete: Some(true),
        };
        assert_eq!(reported(None, Progress::Event), (None, None));
        assert_eq!(
            reported(Some(&report("guest-exit", 195)), Progress::Event),
            (Some(Evidence::RepairStatus), Some(195))
        );
        assert_eq!(
            reported(Some(&report("guest-exit", 54)), Progress::Activating),
            (Some(Evidence::UnusableRoot), Some(54))
        );
        assert_eq!(
            reported(Some(&report("guest-exit", 54)), Progress::Committed),
            (None, Some(54))
        );
        assert_eq!(
            reported(Some(&report("success", 0)), Progress::Committed),
            (None, Some(0))
        );
        // A failure of the VMM, or of its teardown, has no status of the guest's.
        assert_eq!(
            reported(Some(&report("vmm-failure", 1)), Progress::Spawned),
            (None, None)
        );
        assert_eq!(
            reported(Some(&report("teardown-failure", 1)), Progress::Spawned),
            (None, None)
        );
        assert_eq!(
            reported(Some(&report("guest-exit", 256)), Progress::Spawned),
            (None, None)
        );
    }

    #[test]
    fn a_status_prevails_over_what_was_seen_of_the_same_end_but_not_over_the_template_s_evidence() {
        let template = [Evidence::MalformedGeneration, Evidence::TemplateDefect];
        for evidence in ALL {
            // Without a status that is evidence, what the step saw stands.
            assert_eq!(prevailing(evidence, None), evidence, "{evidence}");
            for reported in [
                Evidence::RepairStatus,
                Evidence::UnusableRoot,
                Evidence::Timeout,
            ] {
                let prevailed = prevailing(evidence, Some(reported));
                // A check of the generation, or a diagnosis that attributes a defect to it, shows
                // the provenance that the status alone leaves to establish.
                let expected = if template.contains(&evidence) {
                    evidence
                } else {
                    reported
                };
                assert_eq!(prevailed, expected, "{evidence} {reported}");
            }
        }
    }

    #[test]
    fn openvmm_s_fatal_error_is_evidence_of_an_incompatible_host_only_for_its_codes() {
        for code in INCOMPATIBLE_HOST_CODES {
            // OpenVMM leads its fatal error with the first time ABI code of the error's chain.
            let line = format!("fatal error: [{code}] failed to launch vm worker");
            assert_eq!(
                fatal_evidence(&line),
                Some(Evidence::IncompatibleHost),
                "{line}"
            );
        }
        // A code of the snapshot's time contract, or of this OpenVMM, does not tell the
        // generation's defect from the host's, and an error without a code tells nothing.
        for line in [
            "fatal error: [E_MANIFEST_TIME] failed to launch vm worker",
            "fatal error: [E_PROFILE_DIGEST] failed to launch vm worker",
            "fatal error: [E_SNAPSHOT_VERSION] failed to restore",
            "fatal error: failed to open the snapshot",
            "fatal error: [E_TSC_RATE_TOLERANCE failed to launch vm worker",
            // A line that is not the fatal error, such as a warning, names no code of it.
            "NVX-CPU-PROFILE-FALLBACK: [E_PROFILE_UNSUPPORTED] the host profile serves",
            "warning: fatal error: [E_CPU_GENERATION] quoted",
        ] {
            assert_eq!(fatal_evidence(line), None, "{line}");
        }
    }

    #[test]
    fn openvmm_s_fatal_error_is_evidence_of_a_forward_bind_when_it_names_a_forward_it_cannot_bind()
    {
        // OpenVMM spells the error as its source, which the excerpt copies, writes it.
        let excerpt =
            include_str!("../../../../tests/vectors/openvmm/port_forward_socket_creation.rs");
        let attribute = format!("#[error(\"{FORWARD_BIND_TEXT}{{details}})\")]");
        assert!(
            excerpt.lines().any(|line| line.trim() == attribute),
            "{attribute} in {excerpt}"
        );
        // The cause of the chain follows the line that leads the error, or is the line's own.
        for line in [
            "fatal error: failed to launch vm worker: failed to create socket for port forward \
             (Tcp 127.0.0.1:18080)",
            "fatal error: failed to create socket for port forward (Udp 127.0.0.1:5353)",
            "fatal error: [E_MANIFEST_TIME] failed to launch vm worker: failed to create socket \
             for port forward (Tcp 127.0.0.1:18080)",
        ] {
            assert_eq!(fatal_evidence(line), Some(Evidence::ForwardBind), "{line}");
        }
        // A code of an incompatible host prevails, and a line that is not the fatal error tells
        // nothing.
        let incompatible = "fatal error: [E_CPU_GENERATION] failed to launch vm worker: failed to \
                            create socket for port forward (Tcp 127.0.0.1:18080)";
        assert_eq!(
            fatal_evidence(incompatible),
            Some(Evidence::IncompatibleHost)
        );
        let quoted = "warning: failed to create socket for port forward (Tcp 127.0.0.1:18080)";
        assert_eq!(fatal_evidence(quoted), None);
    }

    #[test]
    fn host_failures_that_ran_out_of_a_resource_are_resource_exhaustion() {
        let failed = |kind: io::ErrorKind| {
            host_evidence(&Error::io("cannot write").with_source(io::Error::from(kind)))
        };
        assert_eq!(
            failed(io::ErrorKind::StorageFull),
            Evidence::ResourceExhaustion
        );
        assert_eq!(
            failed(io::ErrorKind::OutOfMemory),
            Evidence::ResourceExhaustion
        );
        assert_eq!(
            failed(io::ErrorKind::PermissionDenied),
            Evidence::HostFailure
        );
        assert_eq!(
            host_evidence(&Error::io("no source")),
            Evidence::HostFailure
        );
        // A cause deeper in the chain counts too.
        let nested = Error::io("outer").with_source(
            Error::io("inner").with_source(io::Error::from(io::ErrorKind::StorageFull)),
        );
        assert_eq!(host_evidence(&nested), Evidence::ResourceExhaustion);
    }

    #[test]
    fn teardowns_follow_the_end_of_the_launch() {
        assert_eq!(Teardown::of(Ok(Recovery::Exited)), Teardown::Exited);
        assert_eq!(Teardown::of(Ok(Recovery::Clean)), Teardown::Exited);
        assert_eq!(
            Teardown::of(Ok(Recovery::Indeterminate("held".to_owned()))),
            Teardown::Unresolved("held".to_owned())
        );
        let failed = Teardown::of(Err(Error::io("cannot read")));
        assert!(!failed.is_resolved());
        assert!(!Teardown::Pending.is_resolved());
        assert!(Teardown::NoVm.is_resolved());
        assert!(Teardown::Terminated.is_resolved());
    }
}
