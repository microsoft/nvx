//! The launch of a clone, [`ClonePlan::launch`]: OpenVMM's restore of the snapshot of the leased
//! generation, as the launch that the clone's lease reserves, with the clone's devices, its
//! activation record, and the endpoint at which the launch awaits the VMM's restore-ready event.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::failure::{
    Attribute, CloneFailure, Evidence, FailureRecord, Fault, Progress, Stage, Teardown,
    host_evidence,
};
use super::records::{End, ProgressLog, Recorder, Tracker, abandon};
use super::{ClonePlan, CloneUpper, MIB};
use crate::activation::ActivationRecord;
use crate::error::{Error, ErrorKind, Result};
use crate::host::artifacts;
use crate::host::filesystem;
use crate::host::platform::{self, ReadyListener};
use crate::host::sandbox::SandboxPlan;
use crate::host::sandbox::distro::CheckedDistro;
use crate::host::store::files::write_private;
use crate::host::store::{LockedStore, Recovery, RuntimeIdentity};
use crate::host::supervisor::{self, Inherited, Starting, VmmCommand};
use crate::host::template::key::TemplateKey;
use crate::host::template::store::{Acquired, Lease, SOCKET_PATH_LIMIT};
use crate::protocol::{CAPABILITY_LEN, HostMap};

/// A point of a clone's launch at which a test interrupts it, before the step that follows the
/// point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Point {
    /// The sandbox holds the clone's activation record, the launch listens for the restore-ready
    /// event, and the launch has not begun.
    Prepared,
    /// The launch's journal names it, and its VMM is not spawned yet.
    Begun,
}

/// The seams of a clone's launch, through which tests observe and interrupt it.
#[derive(Clone, Copy)]
pub(super) struct Hooks<'a> {
    /// Returns the command that starts the VMM, given the one that the launch made.
    pub(super) command: &'a dyn for<'h> Fn(VmmCommand<'h>) -> VmmCommand<'h>,
    /// Fails the launch at a point, as a test asks.
    pub(super) interrupt: &'a dyn Fn(Point) -> Result<()>,
    /// Ends the launch of a clone that failed after its VMM was spawned.
    pub(super) end: End,
}

impl Hooks<'static> {
    /// The seams of every launch outside tests.
    pub(super) const LAUNCH: Self = Self {
        command: &unchanged,
        interrupt: &uninterrupted,
        end: abandon,
    };
}

/// Returns `command`, as launches outside tests start their VMMs.
fn unchanged(command: VmmCommand<'_>) -> VmmCommand<'_> {
    command
}

/// Lets a launch pass every point, as launches outside tests do.
fn uninterrupted(_: Point) -> Result<()> {
    Ok(())
}

impl ClonePlan {
    /// Launches the clone that the plan describes, under the lock that acquired its lease, which
    /// `acquired` holds, as the launch that the lease reserves: the OpenVMM executable `openvmm`,
    /// which a relative path names from the current directory, restores the leased generation's
    /// snapshot with the clone's devices, under the runtime identity `runtime`, which must be the
    /// template's, [`TemplateKey::runtime`], and receives the plan's capability. Returns the
    /// launch, whose VMM runs, as a [`Restoring`], whose restore sequence,
    /// [`Restoring::activate`], activates the clone, under a deadline that starts as this
    /// launch does.
    ///
    /// The launch first checks, without writing anything, that `acquired` holds the plan's lease,
    /// under the lock that acquired it, before the launch that it reserves began, that the
    /// sandbox's clone binding records the lease: the acquisition made the binding durable, and no
    /// VMM of the clone starts without it, and that the sandbox records no failure of the clone:
    /// a lease's clone fails once, and the record of its failure lasts until the release, even
    /// for a launch that failed before it began, which leaves the lock's reservation. It then:
    ///
    /// 1. Creates the clone's progress log,
    ///    [`CLONE_PROGRESS_NAME`](crate::host::store::CLONE_PROGRESS_NAME) in the sandbox's
    ///    directory, in place of one that an earlier launch left, which it removes through a
    ///    handle of the directory that holds the sandbox's lock, once it removed the same way the
    ///    outcome report that an earlier VM of the sandbox left, so that a report beside the
    ///    clone's records is the clone's VM's. The launch and the restore sequence append to the
    ///    log each step that the clone reaches, without syncing it, as
    ///    [`CloneRecords`](super::CloneRecords) describes.
    /// 2. Writes the clone's activation record, in its canonical encoding, as the file
    ///    [`CLONE_ACTIVATION_NAME`](crate::host::store::CLONE_ACTIVATION_NAME) in the sandbox's
    ///    directory, in place of one that an earlier launch left, before the VMM starts: the
    ///    record, with the transaction ID of its restore section, is the one that the clone
    ///    activates with, which the caller, or a caller that takes the sandbox over after a
    ///    crash, reads there again and resends unchanged.
    /// 3. Listens at the endpoint of the VMM's restore-ready event, which OpenVMM connects to once
    ///    it has prepared the restore, before the restored VM runs, and which must exist before
    ///    the VMM starts: on Linux, the socket
    ///    [`RESTORE_READY_NAME`](crate::host::store::RESTORE_READY_NAME) in the sandbox's
    ///    directory, in place of one that an earlier launch left; on Windows, a named pipe whose
    ///    name the launch draws at random.
    /// 4. Runs [`supervisor::launch`] as the launch that the lease reserves, which begins it with
    ///    the store's launch journal, starts the VMM, and records its identity in the journal.
    /// 5. Appends the spawn to the progress log before it returns, since a teardown of the launch
    ///    removes the journal, which records the spawn too. A launch that cannot append it fails
    ///    at the launch stage, and ends the VMM.
    ///
    /// The VMM restores the generation's snapshot in place, in the generation's payload, which it
    /// only reads, and which the launch neither copies nor changes, so any number of clones
    /// restore one generation at once. OpenVMM requires the control socket of a restore's VMM on
    /// Linux in the snapshot's parent, so the VMM listens beside the snapshot, at the socket that
    /// the lease reserved, `<launch>.sock` in the payload, after the launch's identity, rather
    /// than at the sandbox's own control socket, as
    /// [`Store::acquire`](crate::host::template::Store::acquire) describes; on Windows, at a named
    /// pipe, as every launch does. [`Starting::endpoint`] returns it.
    ///
    /// The VMM's arguments are those of the clone's devices: the microVM machine, the clone's
    /// image as the read-only `distro` block device, which OpenVMM opens at its path, as [Block
    /// devices by path](crate::host::template::Store#block-devices-by-path) describes, and presents
    /// at the capacity that the template's placeholder fixed, reading zeros past the end of a
    /// shorter image; for an upper on a scratch image, the image as the `scratch` block device,
    /// which OpenVMM opens alike, and for an upper in RAM, `--restore-absent-scratch`, which
    /// declares the template's scratch absent, so that OpenVMM attaches it with no file, at the
    /// geometry that the template recorded, as the template's guest negotiated it: its reads return
    /// zeros and its writes fail, and the clone's guest never uses it; and the clone's network, if
    /// the template has a NIC: `--restore-network-policy`, with which the policy of the clone's
    /// network replaces the template's complete deny-all policy on the NIC, and the options of that
    /// policy, without `--net` and the guest's address, which OpenVMM takes from the snapshot. The
    /// clone's export, if it has one, follows, bound to the template's dormant filesystem slot as a
    /// cold launch attaches it, as its [plan](ClonePlan::export) holds it,
    /// [`ExportPlan::openvmm_arguments`](crate::host::filesystem::ExportPlan::openvmm_arguments):
    /// `--mount-aggregate` with [`GUEST_EXPORT`](crate::host::filesystem::GUEST_EXPORT), a
    /// `--mount-child` of each of its directories and files, with its mode, `rw` if a mapping
    /// writes it, and `ro` otherwise, and `file` for a file, then their hidden, re-exposed, and
    /// writable paths, and the caller owner; without an export, the slot stays dormant. Then come
    /// `--single-process`, the template's backend,
    /// CPU profile, and processor capacity, `--restore-snapshot` with the directory of the
    /// generation's snapshot, [`ClonePlan::snapshot`], for a template that records its processors
    /// online at boot, [`TemplateKey::boot_processors`], `--restore-processors` with the processor
    /// target, which brings online those of the target beyond them, `--restore-memory` with the
    /// memory target,
    /// `--restore-ready-path` with the endpoint of the restore-ready event, a boot console that
    /// discards its output, as cold boots have, the control console at the launch's endpoint,
    /// which authenticates with the capability that the VMM reads from its standard input, and the
    /// outcome report, [`SandboxStore::outcome_path`](crate::host::store::SandboxStore::outcome_path).
    /// They name no kernel, initramfs, kernel command line, or RAM, which the snapshot fixes. The
    /// VMM runs in the sandbox's directory.
    ///
    /// The claim of a scratch image passes to the VMM, which holds it until it exits. On Linux,
    /// the VMM inherits the claim's descriptor, which no argument names. On Windows, the claim's
    /// handle, open for writing, would make OpenVMM's open of the image fail, since that open
    /// shares no writing: the launch closes it before the VMM starts, and the VMM inherits the
    /// image's guard instead, which keeps every other claim out, and the image's path naming the
    /// image, while the VMM runs. The [`Restoring`] holds the plan's image, open for reading.
    ///
    /// A crash of the caller at any step leaves the clone's files in the sandbox, which a release
    /// of the lease, [`Store::release`](crate::host::template::Store::release), removes, and a
    /// launch that the journal names, which a reattach ends, as
    /// [Crashes](crate::host::supervisor#crashes) describes, removing the control socket that the
    /// VMM, if it started, left beside the snapshot.
    ///
    /// Like every launch of the sandbox's store, the launch names the sandbox's files by their
    /// paths, under its lock: the progress log that it creates, the activation record that it
    /// writes, the restore-ready socket at which it listens on Linux, and the journal, capability,
    /// and log with which it begins; and the VMM, which runs in the sandbox's directory, opens the
    /// files that its arguments name there. So the directories above the sandbox's must be ones
    /// that only the current user, or an administrator, may change, as the
    /// [store](crate::host::store) requires of them, and as [Block devices by
    /// path](crate::host::template::Store#block-devices-by-path) requires of those above the
    /// clone's block devices. The launch removes the outcome report of an earlier VM of the
    /// sandbox, and the progress log of an earlier launch, through a handle of the directory that
    /// holds the sandbox's lock, though, as a release removes the clone's files, and creates its
    /// own log at its path, where no file may exist: no change of the path makes the launch remove
    /// another sandbox's report or log, and another sandbox's log fails the launch instead, before
    /// it writes anything else.
    ///
    /// Fails with a [`CloneFailure`] at [`Stage::Launch`], whose evidence is the host's for a
    /// failure of its own operations, as [`ClonePlan::new`] describes it, checking in this order:
    ///
    /// - before anything is written, and so recording nothing, as a plan does: with
    ///   [`ErrorKind::Other`](crate::ErrorKind::Other) if `acquired` holds another lease than the
    ///   plan's, under another lock than the one that acquired it, or after the launch that it
    ///   reserves began, if the sandbox's clone binding does not record the plan's lease, and if
    ///   the sandbox records a failure of the clone, [`Evidence::HostFailure`], and as
    ///   [`Lease::of`] fails, or the record of a failure cannot be examined, the host's evidence;
    ///   with
    ///   [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a runtime identity other
    ///   than the template's, [`Evidence::RuntimeMismatch`]; on Linux, with
    ///   [`ErrorKind::Other`](crate::ErrorKind::Other) if the control socket that the lease
    ///   reserves is not beside the plan's snapshot, as for a plan and a lease that name the
    ///   template store by different directories, [`Evidence::HostFailure`], with
    ///   [`ErrorKind::Io`](crate::ErrorKind::Io) if the generation's payload cannot be resolved
    ///   or opened, the host's evidence, or is not a directory of the current user with mode
    ///   0700, as OpenVMM requires of the directory of a control socket,
    ///   [`Evidence::MalformedGeneration`], and with
    ///   [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) if the path of the control
    ///   socket, with or without the payload's links resolved, or of the restore-ready socket, is
    ///   not shorter than 108 bytes, as a Unix socket's address requires, or is not valid UTF-8,
    ///   as OpenVMM and the host take it, or if the path of the control socket holds a comma or an
    ///   equals sign, which OpenVMM's option `listen=<path>` cannot take,
    ///   [`Evidence::EndpointBind`]; as
    ///   [`ExportPlan::verify`](crate::host::filesystem::ExportPlan::verify) fails for the clone's
    ///   export, if it has one: with [`ErrorKind::Other`](crate::ErrorKind::Other) for a
    ///   directory or a file of the export that is no longer the kind of object that the export's
    ///   plan found, a mapped or hidden object that the guest could move that is no longer the
    ///   object that the plan found, or an object that its path no longer reaches, and with
    ///   [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a read-only mapping that
    ///   the host cannot keep read-only, or an owner that the host no longer supports,
    ///   [`Evidence::InvalidMappings`], and with [`ErrorKind::Io`](crate::ErrorKind::Io) if an
    ///   object cannot be inspected otherwise, the host's evidence; with
    ///   [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) for an `openvmm` whose path
    ///   cannot be resolved, the host's evidence; with
    ///   [`ErrorKind::Other`](crate::ErrorKind::Other) if the sandbox's path no longer names the
    ///   directory that holds its lock, and if the progress log cannot be created, as when the
    ///   path names a directory that holds one already, the host's evidence;
    /// - once the progress log exists, recording the failure in the sandbox, as the
    ///   [failures](CloneFailure) of a clone's launch are: with
    ///   [`ErrorKind::Io`](crate::ErrorKind::Io) if the activation record cannot be written, the
    ///   host's evidence, or the endpoint of the restore-ready event cannot be opened,
    ///   [`Evidence::EndpointBind`] unless the host ran out of a resource; and as
    ///   [`supervisor::launch`] fails, which ends the launch, the host's evidence, with the
    ///   teardown [`Teardown::NoVm`] before the spawn of the VMM, [`Teardown::Terminated`] once
    ///   the launch ended the VMM that it spawned through its handle, and
    ///   [`Teardown::Unresolved`] for a launch that could not be ended, whose journal remains.
    ///
    /// A launch that fails leaves the clone's files that it wrote, and the record of its failure,
    /// for the release.
    pub fn launch<'s>(
        self,
        acquired: &'s mut Acquired,
        openvmm: &Path,
        runtime: &RuntimeIdentity,
    ) -> Result<Restoring<'s>, CloneFailure> {
        self.launch_with(acquired, openvmm, runtime, Hooks::LAUNCH)
    }

    /// Launches the clone as [`launch`](Self::launch) does, through `hooks`.
    pub(super) fn launch_with<'s>(
        self,
        acquired: &'s mut Acquired,
        openvmm: &Path,
        runtime: &RuntimeIdentity,
        hooks: Hooks<'_>,
    ) -> Result<Restoring<'s>, CloneFailure> {
        // The restore-to-Ready deadline of the clone's restore sequence starts here.
        let launched = Instant::now();
        let Acquired { lease, locked } = acquired;
        let planned = self.lease.clone();
        let transaction = self.activation.restore().map(|restore| restore.transaction);
        // Until the launch writes a file, the sandbox may not be the clone's, so its failure is
        // recorded nowhere.
        let unrecorded = |fault: Fault| {
            let record = FailureRecord::new(
                &planned,
                transaction,
                (Stage::Launch, Progress::Planned),
                fault.evidence,
                None,
                Teardown::NoVm,
                &fault.error,
            );
            CloneFailure::new(fault.error, record)
        };
        self.check_launch(lease, locked, runtime)
            .map_err(unrecorded)?;
        let openvmm = artifacts::absolute(openvmm).on_host().map_err(unrecorded)?;
        let Self {
            lease: _,
            generation: _,
            key,
            snapshot,
            image,
            upper,
            processors,
            memory,
            activation,
            capability,
            devices,
            export,
        } = self;
        let mut devices = device_arguments(&devices)
            .attribute(Evidence::InvalidSettings)
            .map_err(unrecorded)?;
        let table = match &export {
            Some(export) => {
                devices.extend(export.arguments());
                export.table()
            }
            None => Vec::new(),
        };
        let log = locked
            .open_dir()
            .and_then(|directory| ProgressLog::create(&directory, locked.dir(), lease.launch()))
            .on_host()
            .map_err(unrecorded)?;
        let recorder = Recorder::new(lease.clone(), transaction, locked);
        let mut tracker = Tracker::new(recorder, log, hooks.end);
        let unspawned =
            |tracker: &Tracker, fault: Fault| tracker.record(Stage::Launch, fault, Teardown::NoVm);
        let record = locked.clone_activation_path();
        write_private(&record, &activation.encode())
            .on_host()
            .map_err(|fault| unspawned(&tracker, fault))?;
        let (ready, listener) =
            platform::listen_ready(&locked.restore_ready_path()).map_err(|error| {
                let fault = Fault::host(
                    Error::io(format!(
                        "cannot listen for the restore-ready event of the clone in {}",
                        locked.dir().display()
                    ))
                    .with_source(error),
                );
                let fault = match fault.evidence {
                    Evidence::HostFailure => Fault::new(Evidence::EndpointBind, fault.error),
                    _ => fault,
                };
                unspawned(&tracker, fault)
            })?;
        (hooks.interrupt)(Point::Prepared)
            .on_host()
            .map_err(|fault| unspawned(&tracker, fault))?;
        // On Windows, this closes the claim's handle, and keeps the image's guard.
        let claim = match upper {
            CloneUpper::Scratch(checked) => Some(checked.into_file()),
            CloneUpper::Tmpfs => None,
        };
        // No spawn passes a descriptor of a standard stream, on which the claim is if the caller
        // had closed that stream: it moves above them, with its open file description.
        #[cfg(target_os = "linux")]
        let claim = match claim {
            Some(mut file) => {
                crate::host::sys::move_above_standard_streams(&mut file).map_err(|error| {
                    unspawned(
                        &tracker,
                        Fault::host(
                            Error::io(
                                "cannot pass the claim of the clone's scratch image to its VMM",
                            )
                            .with_source(error),
                        ),
                    )
                })?;
                Some(file)
            }
            None => None,
        };
        let restore = Restore {
            devices,
            key: &key,
            snapshot,
            processors,
            memory,
            ready: &ready,
            report: locked.outcome_path(),
        };
        let dir = locked.dir().to_owned();
        let inherit: Vec<Inherited<'_>> = claim.iter().map(Inherited::new).collect();
        let starting = supervisor::launch_reporting(
            locked,
            lease.launch(),
            runtime,
            &capability,
            |endpoint| {
                (hooks.interrupt)(Point::Begun)?;
                let mut command = VmmCommand::new(&openvmm, restore.arguments(endpoint), &dir);
                command.inherit = inherit;
                Ok((hooks.command)(command))
            },
            |error, spawned| {
                // A VMM that the launch spawned, and could not record, got the clone that far,
                // which only the record of its failure tells once the launch has ended it.
                if spawned {
                    tracker.advance(Progress::Spawned);
                }
                // A failure that cannot be recorded keeps the launch's journal.
                tracker.pending(Stage::Launch, host_evidence(error), error)
            },
        );
        // The VMM holds the claim, if it started.
        drop(claim);
        let starting = match starting {
            Ok(starting) => starting,
            Err(failure) => {
                let teardown = match (failure.unended, failure.spawned) {
                    (Some(reason), _) => Teardown::Unresolved(reason),
                    (None, true) => Teardown::Terminated,
                    (None, false) => Teardown::NoVm,
                };
                return Err(tracker.record(Stage::Launch, Fault::host(failure.error), teardown));
            }
        };
        // The progress log records the spawn before the launch returns: the journal records it too,
        // but a teardown of the launch removes the journal, and a reattach then takes how far the
        // clone got, and so the stage of its failure, from the log.
        if let Err(fault) = tracker.reach(Progress::Spawned) {
            return Err(tracker.fail(starting, Stage::Launch, fault));
        }
        Ok(Restoring {
            starting,
            launched,
            ready,
            listener,
            image,
            activation,
            record,
            capability,
            tracker,
            table,
        })
    }

    /// Refuses a launch of the plan through `lease` and `locked` that is not the one that the
    /// plan's lease reserves, or whose binding the sandbox does not record, one under `runtime`
    /// that the template does not take, and one of an export whose host objects changed since its
    /// plan found them, as
    /// [`launch`](Self::launch) describes, without writing anything.
    fn check_launch(
        &self,
        lease: &Lease,
        locked: &LockedStore,
        runtime: &RuntimeIdentity,
    ) -> Result<(), Fault> {
        let planned = &self.lease;
        if lease != planned
            || locked.dir() != planned.sandbox()
            || locked.reserved() != Some(planned.launch())
        {
            return Err(Fault::new(
                Evidence::HostFailure,
                Error::other(format!(
                    "a clone is launched under the lock that acquired the lease of its plan, {}, \
                     before the launch that the lease reserves begins, but the lock of the \
                     sandbox in {} holds the lease {}, and reserves no launch {} of it",
                    planned.id(),
                    locked.dir().display(),
                    lease.id(),
                    planned.launch()
                )),
            ));
        }
        if Lease::of(locked).on_host()?.as_ref() != Some(planned) {
            return Err(Fault::new(
                Evidence::HostFailure,
                Error::other(format!(
                    "the sandbox in {} records no clone binding of the lease {}, which its \
                     acquisition makes durable before any VMM of the clone starts",
                    locked.dir().display(),
                    planned.id()
                )),
            ));
        }
        // A launch that failed before it began leaves the lock's reservation, but the record of
        // its failure, which no launch removes, ends the clone's launches until the release.
        let failure = locked.clone_failure_path();
        match fs::symlink_metadata(&failure) {
            Ok(_) => {
                return Err(Fault::new(
                    Evidence::HostFailure,
                    Error::other(format!(
                        "the clone of the lease {} failed, as {} records: a lease's clone fails \
                         once, and the release of the lease ends it",
                        planned.id(),
                        failure.display()
                    )),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(Fault::host(
                    Error::io(format!(
                        "cannot examine the record of a failure of the clone {}",
                        failure.display()
                    ))
                    .with_source(error),
                ));
            }
        }
        if runtime != self.key.runtime() {
            return Err(Fault::new(
                Evidence::RuntimeMismatch,
                Error::unsupported(format!(
                    "the template's generation {} restores under the runtime that captured it, \
                     {:?}, not under {runtime:?}",
                    self.generation.generation(),
                    self.key.runtime()
                )),
            ));
        }
        if cfg!(target_os = "linux")
            && let Some(socket) = locked.reserved_socket()
        {
            self.check_sockets(socket, &locked.restore_ready_path())?;
        }
        // The objects of the export must still be those that its plan found, before each restore,
        // as before each cold start: a refusal is of the request's mappings, which no longer name
        // what they named, as for an object that moved or is missing, and any other failure is the
        // host's own.
        if let Some(export) = &self.export {
            export.plan().verify().map_err(|error| match error.kind() {
                ErrorKind::Other | ErrorKind::Unsupported => {
                    Fault::new(Evidence::InvalidMappings, error)
                }
                _ => Fault::host(error),
            })?;
        }
        Ok(())
    }

    /// Refuses, on Linux, a control socket of the clone's VMM at `socket`, beside the plan's
    /// snapshot, and a socket of the restore-ready event at `ready`, that OpenVMM or the host
    /// could not use, as [`launch`](Self::launch) describes, without writing anything.
    ///
    /// OpenVMM requires the control socket of a restore in the snapshot's parent, a directory of
    /// the current user with mode 0700, which the payload of a generation is, and listens at the
    /// socket's path with the parent's links resolved, while the host connects at the path that it
    /// recorded: each must be shorter than 108 bytes, as a Unix socket's address requires, and
    /// valid UTF-8, as OpenVMM and the host take them, as must the path of the restore-ready
    /// socket. OpenVMM takes the control socket's path in its option `listen=<path>`, among
    /// options that commas separate, and whose values follow an equals sign, so the path must hold
    /// neither.
    fn check_sockets(&self, socket: &Path, ready: &Path) -> Result<(), Fault> {
        let (Some(payload), Some(name)) = (socket.parent(), socket.file_name()) else {
            return Err(Fault::new(
                Evidence::HostFailure,
                Error::other(format!(
                    "the control socket {} of the clone's VMM names no directory",
                    socket.display()
                )),
            ));
        };
        let resolve = |path: &Path| {
            filesystem::canonicalize(path).map_err(|error| {
                Fault::host(
                    Error::io(format!(
                        "cannot resolve the payload of the generation {}",
                        path.display()
                    ))
                    .with_source(error),
                )
            })
        };
        let canonical = resolve(payload)?;
        let beside = match self.snapshot.parent() {
            Some(parent) if parent == payload => true,
            Some(parent) => resolve(parent)? == canonical,
            None => false,
        };
        if !beside {
            return Err(Fault::new(
                Evidence::HostFailure,
                Error::other(format!(
                    "the control socket {} of the clone's VMM, which its lease reserves, is not \
                     beside the snapshot {} of its plan, where OpenVMM requires it",
                    socket.display(),
                    self.snapshot.display()
                )),
            ));
        }
        let problem = platform::check_store_dir(payload).map_err(|error| {
            Fault::host(
                Error::io(format!(
                    "cannot open the payload of the generation {}",
                    payload.display()
                ))
                .with_source(error),
            )
        })?;
        if let Some(problem) = problem {
            return Err(Fault::new(
                Evidence::MalformedGeneration,
                Error::io(format!(
                    "the payload of the generation {} {problem}",
                    payload.display()
                )),
            ));
        }
        let resolved = canonical.join(name);
        for path in [socket, &resolved, ready] {
            if path.as_os_str().len() >= SOCKET_PATH_LIMIT {
                return Err(Fault::new(
                    Evidence::EndpointBind,
                    Error::unsupported(format!(
                        "the path of the socket {} of a clone's launch must be shorter than \
                         {SOCKET_PATH_LIMIT} bytes, as a Unix socket's address requires",
                        path.display()
                    )),
                ));
            }
        }
        for path in [socket, &resolved, ready] {
            if path.to_str().is_none() {
                return Err(Fault::new(
                    Evidence::EndpointBind,
                    Error::unsupported(format!(
                        "the path of the socket {} of a clone's launch must be valid UTF-8, as \
                         OpenVMM takes it",
                        path.display()
                    )),
                ));
            }
        }
        if socket.to_string_lossy().contains([',', '=']) {
            return Err(Fault::new(
                Evidence::EndpointBind,
                Error::unsupported(format!(
                    "the path of the control socket {} of a clone's VMM must hold no ',' or '=', \
                     which end the path in OpenVMM's option `listen=<path>`",
                    socket.display()
                )),
            ));
        }
        Ok(())
    }
}

/// Returns OpenVMM's arguments of the devices of a clone's restore, `devices`: those that
/// [`SandboxPlan::openvmm_arguments`] renders, with `--restore-network-policy` in place of `--net`
/// and the guest's address, which OpenVMM takes from the snapshot, and refuses beside it, so that
/// the policy of the clone's network replaces the template's complete deny-all policy, then
/// `--restore-absent-scratch` for an upper in RAM, whose plan renders no `scratch` block device.
fn device_arguments(devices: &SandboxPlan) -> Result<Vec<String>> {
    let mut arguments = devices.openvmm_arguments()?;
    if let Some(at) = arguments.iter().position(|argument| argument == "--net") {
        arguments.splice(
            at..(at + 2).min(arguments.len()),
            ["--restore-network-policy".to_owned()],
        );
    }
    if devices.upper().scratch_path().is_none() {
        arguments.push("--restore-absent-scratch".to_owned());
    }
    Ok(arguments)
}

/// What OpenVMM's arguments of a clone's restore name besides its control endpoint.
struct Restore<'a> {
    /// The arguments of the clone's devices, as [`device_arguments`] renders them.
    devices: Vec<String>,
    /// The generation's key.
    key: &'a TemplateKey,
    /// The directory of the snapshot that the VMM restores.
    snapshot: PathBuf,
    /// The processor target.
    processors: u32,
    /// The memory target, in bytes.
    memory: u64,
    /// The endpoint of the restore-ready event.
    ready: &'a str,
    /// The path of the VMM's outcome report.
    report: PathBuf,
}

impl Restore<'_> {
    /// Returns OpenVMM's arguments of the restore, whose control console listens on `endpoint`,
    /// as [`ClonePlan::launch`] describes them.
    fn arguments(&self, endpoint: &str) -> Vec<OsString> {
        let key = self.key;
        let profile = key.cpu_profile();
        let capacity = key.processors().to_string();
        let processors = self.processors.to_string();
        let memory = format!("{}M", self.memory / MIB);
        let listen = format!("listen={endpoint}");
        let mut options = vec![
            OsStr::new("--single-process"),
            OsStr::new("--hypervisor"),
            OsStr::new(key.hypervisor().as_str()),
            OsStr::new("--cpu-profile"),
            OsStr::new(&profile),
            OsStr::new("--processors"),
            OsStr::new(&capacity),
            OsStr::new("--restore-snapshot"),
            self.snapshot.as_os_str(),
        ];
        // A template that records no processors online at boot has all of them online, which
        // OpenVMM restores without the option, and refuses with it.
        if key.boot_processors() != 0 {
            options.extend([OsStr::new("--restore-processors"), OsStr::new(&processors)]);
        }
        options.extend([
            OsStr::new("--restore-memory"),
            OsStr::new(&memory),
            OsStr::new("--restore-ready-path"),
            OsStr::new(self.ready),
            OsStr::new("--virtio-console"),
            OsStr::new("none"),
            OsStr::new("--microvm-control-console"),
            OsStr::new(&listen),
            OsStr::new("--microvm-control-auth-stdin"),
            OsStr::new("--microvm-report"),
            self.report.as_os_str(),
        ]);
        let mut arguments: Vec<OsString> = self.devices.iter().map(OsString::from).collect();
        arguments.extend(options.into_iter().map(OsStr::to_owned));
        arguments
    }
}

/// The launch of a clone whose VMM restores the leased generation's snapshot, as
/// [`ClonePlan::launch`] returns it: the [`Starting`] launch of the VMM, whose journal records
/// the VMM's identity, the endpoint at which the launch awaits the VMM's restore-ready event, the
/// clone's activation record and capability, and its image, open for reading. Its restore
/// sequence, [`activate`](Self::activate), then [`Activated::start`](super::Activated::start),
/// takes the clone to its sandbox's start.
///
/// It borrows the sandbox's locked store exclusively, as [`Starting`] does. Dropping it abandons
/// the launch, as [`abandon`](Self::abandon) does, ignoring failures, and stops listening for the
/// restore-ready event; a crash of the caller leaves the launch's journal instead, for
/// [`LockedStore::recover`].
pub struct Restoring<'s> {
    /// The launch, whose journal records the VMM.
    pub(super) starting: Starting<'s>,
    /// When the launch began, at which the restore-to-Ready deadline starts.
    pub(super) launched: Instant,
    /// The endpoint of the restore-ready event, as OpenVMM's `--restore-ready-path` takes it.
    pub(super) ready: String,
    /// The listener of the restore-ready event.
    pub(super) listener: ReadyListener,
    /// The clone's image, open for reading, which the plan checked.
    pub(super) image: CheckedDistro,
    /// The clone's activation record.
    pub(super) activation: ActivationRecord,
    /// The path of the activation record in the sandbox, which the launch wrote.
    pub(super) record: PathBuf,
    /// The capability of the launch.
    pub(super) capability: [u8; CAPABILITY_LEN],
    /// How far the clone got, and how its failures are recorded.
    pub(super) tracker: Tracker,
    /// The clone's mapping table, which its restore sequence sends once the clone is ready: empty
    /// for a clone without an export.
    pub(super) table: Vec<HostMap>,
}

impl<'s> Restoring<'s> {
    /// Returns the launch, whose VMM runs, and whose journal records its identity.
    pub fn starting(&self) -> &Starting<'s> {
        &self.starting
    }

    /// Returns the endpoint at which the launch awaits the restore-ready event of the VMM, as
    /// OpenVMM's `--restore-ready-path` takes it: the path of a Unix socket on Linux, and the name
    /// of a named pipe, `//./pipe/NAME`, on Windows.
    pub fn ready_endpoint(&self) -> &str {
        &self.ready
    }

    /// Returns the clone's activation record, as the sandbox holds it.
    pub fn activation(&self) -> &ActivationRecord {
        &self.activation
    }

    /// Returns the capability of the launch, with which the control console of the VMM
    /// authenticates the caller.
    pub fn capability(&self) -> &[u8; CAPABILITY_LEN] {
        &self.capability
    }

    /// Returns the clone's image, open for reading, which the plan checked.
    pub fn image(&self) -> &CheckedDistro {
        &self.image
    }

    /// Ends the launch, as [`Starting::abandon`] does, which terminates its VMM and ends the
    /// launch once nothing of it runs, and stops listening for the restore-ready event.
    pub fn abandon(self) -> Result<Recovery> {
        self.starting.abandon()
    }

    /// Returns the launch, which a test commits as a clone's restore does once its VMM runs.
    #[cfg(test)]
    pub(super) fn into_starting(self) -> Starting<'s> {
        self.starting
    }
}

impl fmt::Debug for Restoring<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The capability authenticates control sessions, so it is never shown.
        formatter
            .debug_struct("Restoring")
            .field("starting", &self.starting)
            .field("ready", &self.ready)
            .field("listener", &self.listener)
            .field("image", &self.image)
            .field("activation", &self.activation)
            .finish_non_exhaustive()
    }
}
