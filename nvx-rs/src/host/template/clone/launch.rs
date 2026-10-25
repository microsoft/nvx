//! The launch of a clone, [`ClonePlan::launch`]: OpenVMM's restore of the snapshot of the leased
//! generation, as the launch that the clone's lease reserves, with the clone's devices, its
//! activation record, and the endpoint at which the launch awaits the VMM's restore-ready event.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};

use super::{ClonePlan, CloneUpper, MIB};
use crate::activation::ActivationRecord;
use crate::error::{Error, Result};
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
use crate::protocol::CAPABILITY_LEN;

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
}

impl Hooks<'static> {
    /// The seams of every launch outside tests.
    pub(super) const LAUNCH: Self = Self {
        command: &unchanged,
        interrupt: &uninterrupted,
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
    /// launch, whose VMM runs, as a [`Restoring`].
    ///
    /// The launch first checks, without writing anything, that `acquired` holds the plan's lease,
    /// under the lock that acquired it, before the launch that it reserves began, and that the
    /// sandbox's clone binding records the lease: the acquisition made the binding durable, and no
    /// VMM of the clone starts without it. It then:
    ///
    /// 1. Writes the clone's activation record, in its canonical encoding, as the file
    ///    [`CLONE_ACTIVATION_NAME`](crate::host::store::CLONE_ACTIVATION_NAME) in the sandbox's
    ///    directory, in place of one that an earlier launch left, before the VMM starts: the
    ///    record, with the transaction ID of its restore section, is the one that the clone
    ///    activates with, which the caller, or a caller that takes the sandbox over after a
    ///    crash, reads there again and resends unchanged.
    /// 2. Listens at the endpoint of the VMM's restore-ready event, which OpenVMM connects to once
    ///    it has prepared the restore, before the restored VM runs, and which must exist before
    ///    the VMM starts: on Linux, the socket
    ///    [`RESTORE_READY_NAME`](crate::host::store::RESTORE_READY_NAME) in the sandbox's
    ///    directory, in place of one that an earlier launch left; on Windows, a named pipe whose
    ///    name the launch draws at random.
    /// 3. Runs [`supervisor::launch`] as the launch that the lease reserves, which begins it with
    ///    the store's launch journal, starts the VMM, and records its identity.
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
    /// image as the read-only `distro` block device and the file of its `scratch` block device,
    /// which OpenVMM opens at their paths, as [Block devices by
    /// path](crate::host::template::Store#block-devices-by-path) describes, and the policy of the
    /// template's NIC, if it has one, without the guest's address, which OpenVMM takes from the
    /// snapshot. Then come `--single-process`, the template's backend, CPU profile, and
    /// processors, `--restore-snapshot` with the directory of the generation's snapshot,
    /// [`ClonePlan::snapshot`], `--restore-memory` with the memory target, `--restore-ready-path`
    /// with the endpoint of the restore-ready event, a boot console that discards its output, as
    /// cold boots have, the control console at the launch's endpoint, which authenticates with
    /// the capability that the VMM reads from its standard input, and the outcome report,
    /// [`SandboxStore::outcome_path`](crate::host::store::SandboxStore::outcome_path). They name
    /// no kernel, initramfs, kernel command line, or RAM, which the snapshot fixes. The VMM runs
    /// in the sandbox's directory.
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
    /// Fails, and starts no VMM, checking in this order:
    ///
    /// - with [`ErrorKind::Other`](crate::ErrorKind::Other) if `acquired` holds another lease
    ///   than the plan's, under another lock than the one that acquired it, or after the launch
    ///   that it reserves began, and if the sandbox's clone binding does not record the plan's
    ///   lease, and as [`Lease::of`] fails, before anything is written;
    /// - with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a runtime identity
    ///   other than the template's;
    /// - on Linux, with [`ErrorKind::Other`](crate::ErrorKind::Other) if the control socket that
    ///   the lease reserves is not beside the plan's snapshot, as for a plan and a lease that
    ///   name the template store by different directories; with
    ///   [`ErrorKind::Io`](crate::ErrorKind::Io) if the generation's payload cannot be resolved
    ///   or opened, or is not a directory of the current user with mode 0700, as OpenVMM requires
    ///   of the directory of a control socket; and with
    ///   [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) if the path of the control
    ///   socket, with or without the payload's links resolved, or of the restore-ready socket, is
    ///   not shorter than 108 bytes, as a Unix socket's address requires;
    /// - with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) for an `openvmm` whose
    ///   path cannot be resolved;
    /// - with [`ErrorKind::Io`](crate::ErrorKind::Io) if the activation record cannot be written,
    ///   or the endpoint of the restore-ready event cannot be opened;
    /// - and as [`supervisor::launch`] fails, which ends the launch.
    ///
    /// A launch that fails leaves the clone's files that it wrote for the release.
    pub fn launch<'s>(
        self,
        acquired: &'s mut Acquired,
        openvmm: &Path,
        runtime: &RuntimeIdentity,
    ) -> Result<Restoring<'s>> {
        self.launch_with(acquired, openvmm, runtime, Hooks::LAUNCH)
    }

    /// Launches the clone as [`launch`](Self::launch) does, through `hooks`.
    pub(super) fn launch_with<'s>(
        self,
        acquired: &'s mut Acquired,
        openvmm: &Path,
        runtime: &RuntimeIdentity,
        hooks: Hooks<'_>,
    ) -> Result<Restoring<'s>> {
        let Acquired { lease, locked } = acquired;
        self.check_launch(lease, locked, runtime)?;
        let openvmm = artifacts::absolute(openvmm)?;
        let Self {
            lease: _,
            generation: _,
            key,
            snapshot,
            image,
            upper,
            memory,
            activation,
            capability,
            devices,
        } = self;
        let devices = device_arguments(&devices)?;
        write_private(&locked.clone_activation_path(), &activation.encode())?;
        let (ready, listener) =
            platform::listen_ready(&locked.restore_ready_path()).map_err(|error| {
                Error::io(format!(
                    "cannot listen for the restore-ready event of the clone in {}",
                    locked.dir().display()
                ))
                .with_source(error)
            })?;
        (hooks.interrupt)(Point::Prepared)?;
        // On Windows, this closes the claim's handle, and keeps the image's guard.
        let claim = match upper {
            CloneUpper::Scratch(checked) => Some(checked.into_file()),
            CloneUpper::Tmpfs { .. } => None,
        };
        // No spawn passes a descriptor of a standard stream, on which the claim is if the caller
        // had closed that stream: it moves above them, with its open file description.
        #[cfg(target_os = "linux")]
        let claim = match claim {
            Some(mut file) => {
                crate::host::sys::move_above_standard_streams(&mut file).map_err(|error| {
                    Error::io("cannot pass the claim of the clone's scratch image to its VMM")
                        .with_source(error)
                })?;
                Some(file)
            }
            None => None,
        };
        let restore = Restore {
            devices,
            key: &key,
            snapshot,
            memory,
            ready: &ready,
            report: locked.outcome_path(),
        };
        let dir = locked.dir().to_owned();
        let inherit: Vec<Inherited<'_>> = claim.iter().map(Inherited::new).collect();
        let starting =
            supervisor::launch(locked, lease.launch(), runtime, &capability, |endpoint| {
                (hooks.interrupt)(Point::Begun)?;
                let mut command = VmmCommand::new(&openvmm, restore.arguments(endpoint), &dir);
                command.inherit = inherit;
                Ok((hooks.command)(command))
            });
        // The VMM holds the claim, if it started.
        drop(claim);
        Ok(Restoring {
            starting: starting?,
            ready,
            listener,
            image,
            activation,
            capability,
        })
    }

    /// Refuses a launch of the plan through `lease` and `locked` that is not the one that the
    /// plan's lease reserves, or whose binding the sandbox does not record, and one under
    /// `runtime` that the template does not take, as [`launch`](Self::launch) describes, without
    /// writing anything.
    fn check_launch(
        &self,
        lease: &Lease,
        locked: &LockedStore,
        runtime: &RuntimeIdentity,
    ) -> Result<()> {
        let planned = &self.lease;
        if lease != planned
            || locked.dir() != planned.sandbox()
            || locked.reserved() != Some(planned.launch())
        {
            return Err(Error::other(format!(
                "a clone is launched under the lock that acquired the lease of its plan, {}, \
                 before the launch that the lease reserves begins, but the lock of the sandbox in \
                 {} holds the lease {}, and reserves no launch {} of it",
                planned.id(),
                locked.dir().display(),
                lease.id(),
                planned.launch()
            )));
        }
        if Lease::of(locked)?.as_ref() != Some(planned) {
            return Err(Error::other(format!(
                "the sandbox in {} records no clone binding of the lease {}, which its \
                 acquisition makes durable before any VMM of the clone starts",
                locked.dir().display(),
                planned.id()
            )));
        }
        if runtime != self.key.runtime() {
            return Err(Error::unsupported(format!(
                "the template's generation {} restores under the runtime that captured it, {:?}, \
                 not under {runtime:?}",
                self.generation.generation(),
                self.key.runtime()
            )));
        }
        if cfg!(target_os = "linux")
            && let Some(socket) = locked.reserved_socket()
        {
            self.check_sockets(socket, &locked.restore_ready_path())?;
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
    /// recorded: each must be shorter than 108 bytes, as a Unix socket's address requires.
    fn check_sockets(&self, socket: &Path, ready: &Path) -> Result<()> {
        let (Some(payload), Some(name)) = (socket.parent(), socket.file_name()) else {
            return Err(Error::other(format!(
                "the control socket {} of the clone's VMM names no directory",
                socket.display()
            )));
        };
        let resolve = |path: &Path| {
            filesystem::canonicalize(path).map_err(|error| {
                Error::io(format!(
                    "cannot resolve the payload of the generation {}",
                    path.display()
                ))
                .with_source(error)
            })
        };
        let canonical = resolve(payload)?;
        let beside = match self.snapshot.parent() {
            Some(parent) if parent == payload => true,
            Some(parent) => resolve(parent)? == canonical,
            None => false,
        };
        if !beside {
            return Err(Error::other(format!(
                "the control socket {} of the clone's VMM, which its lease reserves, is not beside \
                 the snapshot {} of its plan, where OpenVMM requires it",
                socket.display(),
                self.snapshot.display()
            )));
        }
        let problem = platform::check_store_dir(payload).map_err(|error| {
            Error::io(format!(
                "cannot open the payload of the generation {}",
                payload.display()
            ))
            .with_source(error)
        })?;
        if let Some(problem) = problem {
            return Err(Error::io(format!(
                "the payload of the generation {} {problem}",
                payload.display()
            )));
        }
        for path in [socket, &canonical.join(name), ready] {
            if path.as_os_str().len() >= SOCKET_PATH_LIMIT {
                return Err(Error::unsupported(format!(
                    "the path of the socket {} of a clone's launch must be shorter than \
                     {SOCKET_PATH_LIMIT} bytes, as a Unix socket's address requires",
                    path.display()
                )));
            }
        }
        Ok(())
    }
}

/// Returns OpenVMM's arguments of the devices of a clone's restore, `devices`: those that
/// [`SandboxPlan::openvmm_arguments`] renders, without `--net` and the guest's address, which
/// OpenVMM takes from the snapshot, and refuses beside it.
fn device_arguments(devices: &SandboxPlan) -> Result<Vec<String>> {
    let mut arguments = devices.openvmm_arguments()?;
    if let Some(at) = arguments.iter().position(|argument| argument == "--net") {
        arguments.drain(at..(at + 2).min(arguments.len()));
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
        let processors = key.processors().to_string();
        let memory = format!("{}M", self.memory / MIB);
        let listen = format!("listen={endpoint}");
        let mut arguments: Vec<OsString> = self.devices.iter().map(OsString::from).collect();
        for argument in [
            OsStr::new("--single-process"),
            OsStr::new("--hypervisor"),
            OsStr::new(key.hypervisor().as_str()),
            OsStr::new("--cpu-profile"),
            OsStr::new(&profile),
            OsStr::new("--processors"),
            OsStr::new(&processors),
            OsStr::new("--restore-snapshot"),
            self.snapshot.as_os_str(),
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
        ] {
            arguments.push(argument.to_owned());
        }
        arguments
    }
}

/// The launch of a clone whose VMM restores the leased generation's snapshot, as
/// [`ClonePlan::launch`] returns it: the [`Starting`] launch of the VMM, whose journal records
/// the VMM's identity, the endpoint at which the launch awaits the VMM's restore-ready event, the
/// clone's activation record and capability, and its image, open for reading.
///
/// It borrows the sandbox's locked store exclusively, as [`Starting`] does. Dropping it abandons
/// the launch, as [`abandon`](Self::abandon) does, ignoring failures, and stops listening for the
/// restore-ready event; a crash of the caller leaves the launch's journal instead, for
/// [`LockedStore::recover`].
pub struct Restoring<'s> {
    /// The launch, whose journal records the VMM.
    starting: Starting<'s>,
    /// The endpoint of the restore-ready event, as OpenVMM's `--restore-ready-path` takes it.
    ready: String,
    /// The listener of the restore-ready event.
    listener: ReadyListener,
    /// The clone's image, open for reading, which the plan checked.
    image: CheckedDistro,
    /// The clone's activation record.
    activation: ActivationRecord,
    /// The capability of the launch.
    capability: [u8; CAPABILITY_LEN],
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
