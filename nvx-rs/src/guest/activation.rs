//! Templates and clones in the guest: the activation of each clone that a restore of a
//! template's snapshot starts, once its capture repaired it, for a guest init or agent, whether
//! or not it is PID 1.
//!
//! A platform template is a VM whose snapshot holds no tenant's root, so that every clone that a
//! restore of the snapshot starts can attach its own image, upper, and export. Its guest captures
//! through [`snapshot::capture`](crate::guest::snapshot::capture), with nothing mounted from the
//! devices that its clones replace, in one call that, in each clone that a restore of the
//! snapshot starts, also repairs the restore before it returns the [`Repaired`] clone, as the
//! [`snapshot`](crate::guest::snapshot) module tells. A failure names the [`CaptureStage`] that
//! failed and its [`CaptureCause`].
//!
//! # Repair
//!
//! In a clone, the capture repairs the restore before it returns, and before anything else runs,
//! in order:
//!
//! 1. checks that the restore holds its seed, and that its capture did not finish it;
//! 2. reads the online CPUs, which must be a prefix of the CPUs, those that the clone captured
//!    online. With a processor target, which may not be below them, it brings each CPU of the
//!    target's prefix that is offline online, through sysfs, as `nvx-snapshot` does, and verifies
//!    that the online CPUs are the target's prefix. Those are CPUs that the VM has had since its
//!    boot, offline until then, so no device is plugged;
//! 3. with a memory target, checks it before it adds any memory block: the kernel's memory block
//!    size must be 128 MiB, and the restore's ranges, aligned to it, must continue the RAM that
//!    the clone captured, in logical order: the first starts where the blocks that the kernel
//!    had before the activation end, and low RAM, which ends at 3 GiB, continues as high RAM at
//!    4 GiB, so that there is at most one range of each, and none enters the MMIO aperture
//!    between them. The host bounds the target by the snapshot's RAM capacity, of which the
//!    guest has no record: OpenVMM's restore refuses a target above it, in
//!    `validate_restore_memory_target`, as the host's plan of the clone does before its launch.
//!    The repair then adds each block of the ranges that the kernel lacks, through its memory
//!    probe interface, onlines each that is not online, and verifies that it is, as
//!    `nvx-snapshot` does;
//! 4. reseeds the kernel's random number generator with the seed, through
//!    [`reseed::reseed`](crate::guest::reseed::reseed), then drops the seed, which zeroes its
//!    bytes. Nothing before draws randomness, from `getrandom` or from `/dev/urandom`;
//! 5. finishes the restore through the time ABI, with the acknowledgement when
//!    the host gates its input until then, and the CPUs that it brought online, which hands the
//!    deferred time work to the time daemon: the restore checks, of the new CPUs too, the stall
//!    detectors' grace period and release, and the wall-clock discipline's fast cadence. The
//!    caller starts the daemon with [`TimeAbi::start_daemon`] at boot, before the capture, and
//!    supervises it. A finish without a running daemon fails with a violation of the time ABI's
//!    check C10, and in report-only mode, it runs the work itself before it returns. The work
//!    may end after the clone is ready, and its failures are fatal repair failures: the daemon
//!    reports them as violations of the time ABI, which stop the VM, with the time ABI's status,
//!    unless the ABI is report-only, since the daemon enforces it.
//!
//! It returns the [`Repaired`] clone: its new VM generation ID, its number of online CPUs, the
//! size of its online memory blocks, and when it acknowledged the restore, which an activation
//! checks against the resources that it expects. The repair never brings a CPU online twice nor
//! adds a memory block twice, and runs the reseed and the finish at most once. A failure is a
//! [`RepairError`], which names the [`RepairStage`] that failed, its [`RepairCause`], and the
//! repair's [`RepairProgress`], and selects the status with which the caller ends the VM,
//! [`RepairError::status`], which `nvx::protocol::status` documents: the capture fails with it,
//! as a [`CaptureCause::Repair`], and the guest must end.
//!
//! # Strict mode
//!
//! The capture and the repair take the time ABI in strict mode, [`TimeAbi::strict`], unless the
//! kernel command line makes it report-only: a violation of the ABI fails them, after its
//! violation event, with [`TimeError::Violation`], whose
//! [`status`](crate::guest::time::Violation::status) is the one with which the ABI would have
//! stopped the VM, rather than stopping the VM, so that the caller owns the VM's exit status and
//! its shutdown. A capture request fails so in a clone whose restore the time ABI could not
//! repair, with 195, as a [`CaptureStage::Capture`] failure, and a repair, with 195 or 193, as
//! [`RepairError::status`] tells. The time daemon enforces the ABI all the same: a violation that
//! it reports stops the VM, unless the ABI is report-only.
//!
//! # Activation
//!
//! A repaired clone then takes its own configuration from the host, in one Activate, on a
//! restricted control session, through an [`Activation`] of the [`Repaired`] clone. Activation
//! holds no admission fence and makes no system call. It is independent of any framing: the adapter
//! passes it what it receives on the clone's NVXS session, each an [`ActivationInput`], and does
//! what each [`ActivationAction`] that it returns says. Its [`ActivationState`]s, in order, are:
//!
//! 1. the transport bootstrap: the clone awaits the reset that the restore queued for the session
//!    that the guest attached before the capture, which the adapter acknowledges;
//! 2. restricted: the session takes only flow control and one Activate, and anything else, a
//!    request or another reset, fails the activation. The activation decodes the Activate's
//!    record with [`ActivationRecord::decode`](crate::activation::ActivationRecord::decode), and
//!    accepts it only as a clone's, with a restore section, that expects the CPU count and the
//!    memory size that the repair reports, before anything has a side effect. An Activate whose
//!    record the adapter's framing refused unread, as longer than a record can be, fails as a
//!    record that long does;
//! 3. activating: the adapter applies the accepted record with `apply`, which
//!    activates the root and the workload with its settings, as below, passes the activation
//!    what the session received meanwhile, which came before Ready, then reports the result to
//!    [`Activation::complete`];
//! 4. ready, with the mappings pending: the activation completed and returned
//!    what Ready reports beside the adapter's own identities, such as its build, a
//!    [`ReadyActivation`](crate::activation::ReadyActivation): that the activation applied, with
//!    the record's transaction ID. The host's mapping table, which the record announced, and the
//!    start of the sandbox follow in the adapter's order, as after a cold boot;
//! 5. started: the adapter's order started the sandbox, which [`Activation::start`] records.
//!
//! The host retries an Activate with the same transaction ID and a byte-equal record, and such a
//! retry joins the activation while it runs, or gets its cached result afterwards, without
//! activating again, also in a later session, such as once the host's attachment ended, whose
//! reset a ready clone takes. A changed record, another transaction, another reset before the
//! clone is ready, and an activation that failed, wholly or partway, are final. The activation's
//! deadline, which the caller chooses, starts at the restore's acknowledgement, or, for a restore
//! that the host did not gate until one, when the caller makes the activation, and neither input
//! nor retries extend it: the adapter wakes by it, and [ticks](Activation::tick) the activation
//! then, however slowly its input trickles in, and whether or not its output drains. A failure
//! is an [`ActivationFailure`], which names the [`ActivationState`] in which the activation failed
//! and its [`ActivationCause`], and which every later call returns again. The activation never
//! powers the VM off: the adapter reports the failure, and ends the VM.
//!
//! The [guest-agent guidelines] give the order of a restore around the activation, from
//! [`Activation::new`] to Ready, with nvx-init as the worked example, and the obligations of
//! its caller, which the activation enforces none of.
//!
//! [guest-agent guidelines]: https://github.com/microsoft/nvx/blob/dev/doc/guest-agent-guidelines.md
//!
//! The activation's application, `apply` and its types, is NVX's internal reference code of the
//! guest's space, hidden from this documentation and unstable: it becomes crate-private once
//! nothing outside NVX uses it.
//!
//! `apply` activates the clone as at a cold boot. It assembles the root as
//! `root::assemble` does, with the record's image, an EROFS image whose UUID it checks if the
//! record expects one, or a GPT disk whose GUID it checks if the record expects one, its upper, a
//! tmpfs or the scratch, with the record's journal policy, and its overlay mode, and with the
//! inputs that the guest takes from its own configuration, which its template captured, a
//! `RootTemplate`: the runtime directory, the block devices, the host filesystem device's
//! virtio-fs tag, the gateway of the template's NIC, if it has one, and how long the assembly waits
//! for each block device, but never past the activation's deadline, which the adapter passes, so
//! that the application ends by then. For a GPT disk, it first asks the kernel to reread the disk's
//! partition table, once the distro device appeared, by the same deadline: the kernel created the
//! block devices of partitions from the template's placeholder, when it found it, and creates those
//! of the clone's disk then, from the same read-only bytes that the assembly reads and checks, as
//! at a cold boot. The upper must be fresh: the assembly refuses one whose `upper` or `work`
//! directory exists and is not empty, since a clone's root starts as its image alone, while a cold
//! sandbox's scratch keeps its upper across boots; the check reads at most one entry of each, and
//! no file. The record holds no execution identity: the adapter gives each execution the identity
//! that the execution names, as for a sandbox without a workload identity at a cold boot. If the
//! record has an export, it mounts the host
//! export through the template's
//! filesystem device, with `Root::mount_export`, read-write, at
//! `hostfs/root` in the runtime directory, outside the root, where only the guest's root user
//! reaches it: the record names no mode, and OpenVMM limits each of the export's children to its
//! own access. It applies the record's VM-wide view through `view::apply`, with the record's
//! DNS server, if any. The root owns the machine-ID and resolver binds, including any made
//! before a later failure. The view sets the VM's hostname and generates an ID only when the
//! record requests one, after the repair's reseed. Application starts no process. It returns
//! the `Applied` root and workload, which the adapter keeps, with the export's root, from which
//! the adapter maps the host's mapping table once the clone is ready, the network's proxy
//! endpoint, which the adapter tells its workloads as at a cold boot, and the publication and
//! gateway with which the caller starts and supervises ingress. The `teardown` of the `Applied`
//! closes the export's root, then tears the root down with its view binds. An application that
//! fails returns an `ApplyError` instead, after which nothing that it mounted or bound stays
//! mounted, unless the error holds the unmounts that failed.
//!
//! # Example
//!
//! ```no_run
//! use std::time::{Duration, Instant};
//!
//! use nvx::guest::activation::Activation;
//! use nvx::guest::snapshot::{self, Captured, Devices, Disks, Disposition};
//! use nvx::protocol::root::{DISTRO_MMIO_BASE, SCRATCH_MMIO_BASE};
//!
//! // At boot, the agent started the time daemon with `TimeAbi::start_daemon`, and supervises it.
//! let devices = Devices {
//!     distro: DISTRO_MMIO_BASE,
//!     scratch: SCRATCH_MMIO_BASE,
//!     filesystem_tag: "microvm".to_owned(),
//! };
//! let timeout = Duration::from_secs(30);
//! match snapshot::capture(&devices, Disks::Fresh, Instant::now() + timeout) {
//!     Ok(Captured::Restored(repaired)) => {
//!         // A clone continues here, repaired: its memory activated, its kernel reseeded, and its
//!         // restore acknowledged.
//!         println!("{} CPUs, {} bytes of memory", repaired.cpus(), repaired.memory());
//!         // Its fence-free activation takes the host's Activate: the agent drives it with
//!         // what its control session receives and admits executions only after Ready.
//!         let activation = Activation::new(&repaired, timeout, Instant::now());
//!         println!("activate by {:?}", activation.deadline());
//!     }
//!     Ok(Captured::Continued) => println!("the VM continued without a restore"),
//!     Err(failure) => match failure.disposition {
//!         // The guest goes on, and the agent reports the failure.
//!         Disposition::Resumed => eprintln!("{failure}"),
//!         // The agent ends the VM with the failure's status, after an error line.
//!         Disposition::MustEnd { status } => {
//!             eprintln!("{failure}");
//!             std::process::exit(status.into());
//!         }
//!         // A disposition that a later version adds cannot be known to be safe.
//!         _ => std::process::exit(125),
//!     },
//!     // Nor can an outcome that a later version adds.
//!     Ok(_) => std::process::exit(125),
//! }
//! ```
//!
//! # Privileges and effects
//!
//! The operations need the guest's root user and the privileges of the time ABI's calls, and the
//! reseed needs `CAP_SYS_ADMIN`. The capture reads the mount table of the caller's mount
//! namespace, which must be the one where the guest mounts its devices, such as its initial
//! namespace, and the repair reads the kernel's CPU and memory interfaces in sysfs, and writes
//! the states of the CPUs that it brings online and its memory interface. They mount nothing and
//! start no thread, and they start no process and collect no child's status, except a
//! report-only finish without a running time daemon, which runs the deferred time work itself,
//! in a child that it waits for. They never power the VM off, so a caller that is not PID 1 keeps
//! its own reaping, transport, and shutdown. The activation needs no privilege, and
//! makes no system call and changes no admission fence. Its
//! application, `apply`, needs the guest's root user, in the guest's initial mount
//! namespace, as `root::assemble` does: for a GPT disk, it asks the kernel to reread the disk's
//! partition table, which needs `CAP_SYS_ADMIN`; it mounts the root, applies its VM-wide view,
//! and, if it fails, tears it down. It starts no process and generates at most one ID, a
//! machine ID that the record asks for. It mounts the host export of a record that has one, and
//! writes the machine ID's and the resolver's files in the runtime directory, whose binds over
//! the root's `/etc/machine-id` and `/etc/resolv.conf` need Linux 6.8 or later.

mod state;
#[cfg(test)]
mod tests;

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::GenerationId;
use super::root::{self, Paths};
use super::snapshot::DeviceNumber;
use super::sys;
use super::time::{CaptureOutcome, CaptureRequest, PreCaptureError, TimeAbi, TimeError};

pub use self::state::{
    Activation, ActivationAction, ActivationCause, ActivationFailure, ActivationInput,
    ActivationState, MACHINE_ID_FILE, RESOLVER_FILE,
};
#[doc(hidden)]
pub use self::state::{
    Applied, AppliedTeardownError, ApplyError, NetworkError, RootTemplate, apply,
};
pub use super::snapshot::{MOUNT_TABLE_LIMIT, Mounted, Origin};
pub use super::snapshot::{
    RangeFault, RepairCause, RepairError, RepairProgress, RepairStage, Repaired,
};

/// The mount table of the calling process's mount namespace, below procfs.
const MOUNT_TABLE: &str = "self/mountinfo";
/// The directory of the block devices' sysfs entries, below sysfs, which names each by its
/// device number, `MAJ:MIN`.
const BLOCK_DEVICES: &str = "dev/block";

/// A stage of a capture, which a [`CaptureFailure`](crate::guest::snapshot::CaptureFailure)
/// names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CaptureStage {
    /// Closing the caller's fence of workload admission, which fails while it is closed already.
    Fence,
    /// Checking that nothing is mounted from the template's devices or an overlay.
    Mounts,
    /// Waiting for the time ABI's boot checks.
    BootChecks,
    /// Running the time ABI's capture checks, after which the calling thread must hold none of
    /// the permission for the time ABI's ports.
    CaptureChecks,
    /// Saving the stall detectors' settings and suppressing the detectors.
    Suppression,
    /// Reading the VM generation ID.
    GenerationId,
    /// Requesting the capture, and repairing the time of a restore.
    Capture,
    /// Restoring the stall detectors' settings.
    Release,
    /// Repairing the clone's resources, entropy and acknowledgement after a restore.
    Repair,
}

impl fmt::Display for CaptureStage {
    /// Writes what the stage does, such as `check the mount table`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Fence => "fence workload admission",
            Self::Mounts => "check the mount table",
            Self::BootChecks => "wait for the time ABI's boot checks",
            Self::CaptureChecks => "run the time ABI's capture checks",
            Self::Suppression => "suppress the stall detectors",
            Self::GenerationId => "read the VM generation ID",
            Self::Capture => "request the capture",
            Self::Repair => "repair the restored clone",
            Self::Release => "restore the stall detectors",
        })
    }
}

/// Why a stage of a capture failed, which a
/// [`CaptureFailure`](crate::guest::snapshot::CaptureFailure) holds.
#[derive(Debug)]
#[non_exhaustive]
pub enum CaptureCause {
    /// A system call failed with this error: a read of the mount table or of a device's entries
    /// in procfs and sysfs, which names the file, a mount table longer than [`MOUNT_TABLE_LIMIT`]
    /// or of another form than the kernel's, or a save or restore of the stall detectors'
    /// settings.
    Io(io::Error),
    /// Something is mounted from a device from which the capture must find nothing mounted, or
    /// from an overlay.
    Mounted(Mounted),
    /// The stall detectors' settings are saved already, by a capture that is still pending, which
    /// restores them.
    Pending,
    /// A call of the time ABI failed: a violation that a strict or report-only call reports, or a
    /// failure of its ports, its time state, or its entropy file.
    Time(TimeError),
    /// The inline repair of a restored clone failed, with its existing stage and progress.
    Repair(RepairError),
}

impl fmt::Display for CaptureCause {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => formatter.write_str(&sys::describe(error)),
            Self::Mounted(mounted) => mounted.fmt(formatter),
            Self::Pending => formatter.write_str("a capture is already pending"),
            Self::Time(error) => error.fmt(formatter),
            Self::Repair(error) => error.fmt(formatter),
        }
    }
}

/// What a capture reads of the kernel and asks of the time ABI, which the tests' fake replaces.
pub(super) trait System: Send {
    /// Reads the mount table of the calling process's mount namespace, at most one byte more than
    /// [`MOUNT_TABLE_LIMIT`] of it.
    fn mount_table(&mut self) -> io::Result<Vec<u8>>;

    /// Returns the canonical sysfs directories of the virtio MMIO windows at `mmio` whose devices
    /// the kernel added, below which it creates their block devices and partitions.
    fn windows(&mut self, mmio: u64) -> io::Result<Vec<PathBuf>>;

    /// Returns the canonical sysfs directory of the block device whose number is `device`.
    fn block_device(&mut self, device: DeviceNumber) -> io::Result<PathBuf>;

    /// Prepares strictly by the caller deadline, retaining suppression ownership on failure.
    fn pre_capture_until(&mut self, deadline: Instant) -> Result<(), (PreCaptureError, bool)>;

    /// Restores the stall detectors' settings, as [`TimeAbi::cancel_capture`] does.
    fn cancel_capture(&mut self) -> Result<(), Vec<u8>>;

    /// Reads the VM generation ID, as [`TimeAbi::generation_id`] does.
    fn generation_id(&mut self) -> Result<GenerationId, TimeError>;

    /// Requests a capture, as [`TimeAbi::capture`] does.
    fn capture(&mut self, request: CaptureRequest) -> Result<CaptureOutcome, (TimeError, bool)>;
}

/// The guest's kernel, at the paths of its interfaces, and its time ABI.
struct LiveSystem {
    /// The directories of the kernel's interfaces.
    paths: Paths,
    /// The time ABI of [`live_abi`].
    abi: TimeAbi,
    /// The caller's pre-request deadline, cleared before the request.
    deadline: Option<Instant>,
}

/// Returns the live capture seam with an explicit optional pre-request deadline.
pub(super) fn capture_system(deadline: Option<Instant>) -> Box<dyn System> {
    Box::new(LiveSystem {
        paths: Paths::default(),
        abi: live_abi(),
        deadline,
    })
}

/// Returns the time ABI of the capture and of the repair: in strict mode, unless the kernel
/// command line makes it report-only.
pub(super) fn live_abi() -> TimeAbi {
    TimeAbi::new().strict()
}

impl System for LiveSystem {
    fn mount_table(&mut self) -> io::Result<Vec<u8>> {
        sys::FileLock::check_deadline(self.deadline)?;
        let path = self.paths.proc.join(MOUNT_TABLE);
        let mut table = Vec::new();
        File::open(&path)
            .and_then(|file| {
                file.take(MOUNT_TABLE_LIMIT as u64 + 1)
                    .read_to_end(&mut table)
            })
            .map_err(|error| at(&path, &error))?;
        sys::FileLock::check_deadline(self.deadline)?;
        Ok(table)
    }

    fn windows(&mut self, mmio: u64) -> io::Result<Vec<PathBuf>> {
        sys::FileLock::check_deadline(self.deadline)?;
        let windows = root::window_directories(&self.paths, mmio)
            .map_err(|error| at(&self.paths.proc.join("iomem"), &error))?;
        let mut directories = Vec::with_capacity(windows.len());
        for window in windows {
            sys::FileLock::check_deadline(self.deadline)?;
            match fs::canonicalize(&window) {
                Ok(directory) => directories.push(directory),
                // The kernel lists the window before it adds the window's device, from which
                // nothing is mounted yet.
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(at(&window, &error)),
            }
        }
        sys::FileLock::check_deadline(self.deadline)?;
        Ok(directories)
    }

    fn block_device(&mut self, device: DeviceNumber) -> io::Result<PathBuf> {
        sys::FileLock::check_deadline(self.deadline)?;
        let path = self.paths.sys.join(BLOCK_DEVICES).join(device.to_string());
        let directory = fs::canonicalize(&path).map_err(|error| at(&path, &error))?;
        sys::FileLock::check_deadline(self.deadline)?;
        Ok(directory)
    }

    fn pre_capture_until(&mut self, deadline: Instant) -> Result<(), (PreCaptureError, bool)> {
        self.abi.pre_capture_until(deadline)
    }

    fn cancel_capture(&mut self) -> Result<(), Vec<u8>> {
        self.abi.cancel_capture_until(self.deadline)
    }

    fn generation_id(&mut self) -> Result<GenerationId, TimeError> {
        self.abi.generation_id_until(self.deadline)
    }

    fn capture(&mut self, request: CaptureRequest) -> Result<CaptureOutcome, (TimeError, bool)> {
        let result = self.abi.capture_until(request, self.deadline);
        if result
            .as_ref()
            .err()
            .is_none_or(|(_, unbounded)| *unbounded)
        {
            self.deadline = None;
        }
        result
    }
}

/// Returns `error`, of an operation on `path`, as an error of the same kind whose text names the
/// path, such as `/proc/self/mountinfo: Permission denied`.
pub(super) fn at(path: &Path, error: &io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("{}: {}", path.display(), sys::describe(error)),
    )
}
