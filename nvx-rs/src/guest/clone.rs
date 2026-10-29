//! Templates and clones in the guest: the capture of a template's guest before its root's
//! assembly, and the repair and the activation of each clone that a restore of its snapshot
//! starts, for a guest init or agent, whether or not it is PID 1.
//!
//! A platform template is a VM whose snapshot holds no tenant's root, so that every clone that a
//! restore of the snapshot starts can attach its own image, upper, and export. Its guest captures
//! through a [`CapturePreparation`], which the caller makes of its own fence of workload
//! admission, an [`AdmissionFence`], and the identities of the [`Devices`] from which the
//! snapshot must hold no mount: the `distro` block device and the scratch block device, which
//! their MMIO windows name, and the host filesystem device, which its virtio-fs tag names.
//! [`CapturePreparation::prepare`], in order:
//!
//! 1. closes the caller's fence, so that no execution starts; a fence that is closed already
//!    fails to close, which stops the preparation;
//! 2. reads the mount table of the caller's mount namespace, `/proc/self/mountinfo`, at most
//!    [`MOUNT_TABLE_LIMIT`] bytes of it, and refuses to capture while anything is mounted from the
//!    distro device or one of its partitions, the scratch device, an overlay, or the filesystem
//!    tag, bind mounts included, with a [`Mounted`] that names the first such mount;
//! 3. waits until the time ABI's boot checks have completed, for 30 seconds at most, runs its
//!    capture checks, and saves the settings of the stall detectors and suppresses them: RCU's
//!    stall warnings, the soft-lockup detector, and the hung-task detector.
//!
//! It returns an owned [`CaptureBarrier`], which holds the fence closed and the detectors
//! suppressed. [`CaptureBarrier::capture`] reads the VM generation ID, and requests the capture
//! through [`TimeAbi::capture`] with the platform tier's request, which keeps no scratch, in the
//! in-memory mode, so that no file holds the fresh entropy that a restore hands each clone. It
//! returns how the capture ended:
//!
//! - [`Captured::Continued`]: the VM continued without a restore, and the barrier opened the
//!   fence, then restored the detectors' settings.
//! - [`Captured::Restored`]: a restore of the snapshot started this VM, a clone whose wall clock
//!   the time ABI repaired, and whose [`Restore`] holds that entropy as its seed. Nothing else may
//!   run before the clone's repair acknowledges the restore, so the fence stays closed:
//!   [`Restored::into_parts`] hands it to the caller, which keeps it closed through the repair,
//!   and opens it once the clone may admit executions, such as once it is activated. The
//!   detectors stay suppressed until the repair finishes the restore, which hands their release
//!   to the time daemon. OpenVMM ends a platform template's VM once its capture commits, so only
//!   clones return with this outcome.
//!
//! [`CaptureBarrier::cancel`] opens the fence, then restores the detectors' settings, and so does
//! a barrier that is dropped, which ignores a failure. A capture that fails restores the
//! detectors' settings too, after which the caller ends the VM, but a request that fails leaves
//! the fence closed: it may fail in a clone whose restore it could not repair. A preparation that
//! fails releases what it took. A failure is a [`CaptureError`], which names the
//! [`CaptureStage`] that failed and its [`CaptureCause`].
//!
//! # Repair
//!
//! A clone repairs its restore next, before anything else runs, with a [`Repair`] of the
//! [`Restore`] that [`Restored::into_parts`] hands over, or with [`repair`](fn@repair), which
//! runs one once. [`Repair::run`], in order:
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
//! 5. finishes the restore through [`TimeAbi::restore_finish`], with the acknowledgement when
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
//! checks against the resources that it expects. The repair records how far it got, so that it
//! never brings a CPU online twice nor adds a memory block twice, and runs the reseed and the
//! finish at most once, whoever retries it: a failure at or after the reseed is final. It never
//! touches the fence. A failure is a [`RepairError`], which names the [`RepairStage`] that
//! failed, its [`RepairCause`], and the repair's [`RepairProgress`], and selects the status with
//! which the caller ends the VM, [`RepairError::status`], which `nvx::protocol::status`
//! documents.
//!
//! # Strict mode
//!
//! The capture and the repair take the time ABI in strict mode, [`TimeAbi::strict`], unless the
//! kernel command line makes it report-only: a violation of the ABI fails them, after its
//! violation event, with [`TimeError::Violation`], whose
//! [`status`](crate::guest::time::Violation::status) is the one with which the ABI would have
//! stopped the VM, rather than stopping the VM, so that the caller owns the VM's exit status and
//! its shutdown. A capture request fails so in a clone whose restore the time ABI could not
//! repair, with 195, as a [`CaptureStage::Capture`] error, and a repair, with 195 or 193, as
//! [`RepairError::status`] tells. The capture's preparation enforces the ABI, as
//! `nvx-time pre-capture` does, and so does the time daemon: a violation of either stops the VM,
//! unless the ABI is report-only.
//!
//! # Activation
//!
//! A repaired clone then takes its own configuration from the host, in one Activate, on a
//! restricted control session, through an [`Activation`] of the [`Repaired`] clone and of the
//! caller's fence, which stays closed until the activation is ready, so that no execution starts
//! before. The activation is a state machine without I/O, independent of any framing: the adapter
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
//! 3. activating: the adapter applies the accepted record with [`apply`](fn@apply), which
//!    activates the root and the workload with its settings, as below, passes the activation
//!    what the session received meanwhile, which came before Ready, then reports the result to
//!    [`Activation::complete`];
//! 4. ready, with the mappings pending: the activation completed, opened the fence, and returned
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
//! [`apply`](fn@apply) activates the clone as at a cold boot. It assembles the root as
//! [`root::assemble`] does, with the record's image, an EROFS image whose UUID it checks if the
//! record expects one, or a GPT disk whose GUID it checks if the record expects one, its upper, a
//! tmpfs or the scratch, with the record's journal policy, and its overlay mode, and with the
//! inputs that the guest takes from its own configuration, which its template captured, a
//! [`RootTemplate`]: the runtime directory, the block devices, the host filesystem device's
//! virtio-fs tag, the gateway of the template's NIC, if it has one, and how long the assembly waits
//! for each block device, but never past the activation's deadline, which the adapter passes, so
//! that the application ends by then. For a GPT disk, it first asks the kernel to reread the disk's
//! partition table, once the distro device appeared, by the same deadline: the kernel created the
//! block devices of partitions from the template's placeholder, when it found it, and creates those
//! of the clone's disk then, from the same read-only bytes that the assembly reads and checks, as
//! at a cold boot. The upper must be fresh: the assembly refuses one whose `upper` or `work`
//! directory exists and is not empty, since a clone's root starts as its image alone, while a cold
//! sandbox's scratch keeps its upper across boots; the check reads at most one entry of each, and
//! no file. If the record has a workload identity, it then checks its account in the assembled
//! root, before Ready, as NVX's init checks it at boot: the identity is
//! [`Identity::new`](super::entry::Identity::new) of the record's user and group IDs, without
//! supplementary groups or capabilities, in existing-account mode, and every execution runs as it.
//! Without one, the adapter gives each execution its own identity, which
//! [`Entry::check`](super::entry::Entry::check) checks. If the record asks for a generated machine
//! ID, it then generates one, once, with
//! [`MachineId::generate`](super::entry::MachineId::generate), after the repair's reseed, so that
//! no two clones of a template share it, to [`MACHINE_ID_FILE`] in the template's runtime
//! directory. If the record has an export, it mounts the host export through the template's
//! filesystem device, with [`Root::mount_export`](root::Root::mount_export), read-write, at
//! `hostfs/root` in the runtime directory, outside the root, where only the guest's root user
//! reaches it: the record names no mode, and OpenVMM limits each of the export's children to its
//! own access. If the record's network names a DNS server, it writes a resolver file that names
//! it to [`RESOLVER_FILE`] in the runtime directory. If the network publishes the workload's
//! listeners, it starts their announcer, with
//! [`Announcer::start`](super::ingress::Announcer::start), to the template's gateway: the
//! template captured its NIC under a policy that publishes nothing, so that no announcer ran in
//! it. It then binds the runtime's files into the root for every execution, with one
//! [`Entry::bind_runtime`](super::entry::Entry::bind_runtime), in the caller's mount namespace:
//! the generated machine ID's file, read-only over the root's `/etc/machine-id`, and the
//! resolver file, read-only over the root's `/etc/resolv.conf`, or, with neither, nothing. The
//! entry checks the record's hostname first, which every execution's entry then takes. It returns
//! the [`Applied`] root and workload, which the adapter keeps, with the export's root, from which
//! the adapter maps the host's mapping table once the clone is ready, the network's proxy
//! endpoint, which the adapter tells its workloads as at a cold boot, and the announcer, which the
//! adapter watches, and must end the clone once the announcer ended: the gateway then keeps
//! publishing the listeners that the announcer last announced until the VM ends, and learns of no
//! later change of them. The [`teardown`](Applied::teardown) of the [`Applied`] stops the
//! announcer, closes the export's root, and removes the binds before it tears the root down. An
//! application that fails returns an [`ApplyError`] instead, after which nothing that it mounted or
//! bound stays mounted, unless the error holds the unmounts that failed, and no announcer that it
//! started runs.
//!
//! # Example
//!
//! ```no_run
//! use std::io;
//! use std::time::{Duration, Instant};
//!
//! use nvx::guest::clone::{
//!     self, Activation, AdmissionFence, CapturePreparation, Captured, Devices,
//! };
//! use nvx::guest::root::Device;
//! use nvx::protocol::root::{DISTRO_MMIO_BASE, SCRATCH_MMIO_BASE};
//!
//! /// The agent's admission of executions, which it checks before it starts one.
//! struct Admission {
//!     open: bool,
//! }
//!
//! impl AdmissionFence for Admission {
//!     fn close(&mut self) -> io::Result<()> {
//!         // Another preparation, or a clone before its repair, holds the fence closed.
//!         if !self.open {
//!             return Err(io::ErrorKind::ResourceBusy.into());
//!         }
//!         self.open = false;
//!         Ok(())
//!     }
//!
//!     fn open(&mut self) {
//!         self.open = true;
//!     }
//! }
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // At boot, the agent started the time daemon with `TimeAbi::start_daemon`, and supervises it.
//! let mut admission = Admission { open: true };
//! let devices = Devices {
//!     distro: Device { mmio: DISTRO_MMIO_BASE, sectors: None },
//!     scratch: Device { mmio: SCRATCH_MMIO_BASE, sectors: None },
//!     filesystem_tag: "microvm".to_owned(),
//! };
//! let barrier = CapturePreparation::new(&mut admission, devices).prepare()?;
//! match barrier.capture()? {
//!     Captured::Continued => println!("the VM continued without a restore"),
//!     Captured::Restored(restored) => {
//!         // A clone continues here, with its wall clock repaired. Its repair activates its
//!         // memory, reseeds it, and acknowledges the restore, while its fence stays closed.
//!         let (restore, fence) = restored.into_parts();
//!         let repaired = match clone::repair(restore) {
//!             Ok(repaired) => repaired,
//!             Err(error) => {
//!                 // The agent ends the VM with the failure's status, after an error line.
//!                 eprintln!("{error} ({})", error.progress());
//!                 std::process::exit(error.status().into());
//!             }
//!         };
//!         println!("{} CPUs, {} bytes of memory", repaired.cpus(), repaired.memory());
//!         // Its activation takes the host's Activate, and opens the fence once the clone is
//!         // ready: the agent drives it with what its control session receives.
//!         let timeout = Duration::from_secs(30);
//!         let activation = Activation::new(&repaired, fence, timeout, Instant::now());
//!         println!("activate by {:?}", activation.deadline());
//!     }
//! }
//! # Ok(())
//! # }
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
//! in a child that it waits for, as [`TimeAbi::restore_finish`] tells. Apart from a violation of
//! the time ABI in the capture's preparation, which stops the VM unless the ABI is report-only,
//! as [strict mode](#strict-mode) tells, they never power the VM off, so a caller that is not
//! PID 1 keeps its own reaping, transport, and shutdown. The activation needs no privilege, and
//! makes no system call: of the clone, it changes only the caller's fence, which it opens. Its
//! application, [`apply`](fn@apply), needs the guest's root user, in the guest's initial mount
//! namespace, as [`root::assemble`] does: for a GPT disk, it asks the kernel to reread the disk's
//! partition table, which needs `CAP_SYS_ADMIN`; it mounts the root, reads the account databases
//! below it, and, if it fails, tears it down. It starts no process but the announcer of a record
//! whose network publishes the workload's listeners, whose socket it binds to a privileged source
//! port, and whose status it collects through the [`Applied`], and it generates at most one ID, a
//! machine ID that the record asks for. It mounts the host export of a record that has one, and
//! writes the machine ID's and the resolver's files in the runtime directory, whose binds over
//! the root's `/etc/machine-id` and `/etc/resolv.conf` need Linux 6.8 or later.

mod activation;
mod mounts;
mod repair;
#[cfg(test)]
mod tests;

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::mem;
use std::path::{Path, PathBuf};

use self::mounts::DeviceNumber;
use super::GenerationId;
use super::root::{self, Device, Paths};
use super::sys;
use super::time::{
    CaptureOutcome, CaptureRequest, PreCaptureError, Restore, ScratchPolicy, TimeAbi, TimeError,
};

pub use self::activation::{
    Activation, ActivationAction, ActivationCause, ActivationFailure, ActivationInput,
    ActivationState, Applied, AppliedTeardownError, ApplyError, MACHINE_ID_FILE, NetworkError,
    RESOLVER_FILE, RootTemplate, apply,
};
pub use self::mounts::{Mounted, Origin};
pub use self::repair::{
    RangeFault, Repair, RepairCause, RepairError, RepairProgress, RepairStage, Repaired, repair,
};

/// The most bytes of the mount table that a [`CapturePreparation`] reads: 1 MiB, the lines of
/// thousands of mounts. It refuses a longer table, rather than check a part of it.
pub const MOUNT_TABLE_LIMIT: usize = 1 << 20;

/// The mount table of the calling process's mount namespace, below procfs.
const MOUNT_TABLE: &str = "self/mountinfo";
/// The directory of the block devices' sysfs entries, below sysfs, which names each by its
/// device number, `MAJ:MIN`.
const BLOCK_DEVICES: &str = "dev/block";

/// The caller's fence of workload admission, which its agent checks before it starts each
/// execution: while the fence is closed, no execution starts.
///
/// A [`CapturePreparation`] closes it before anything else, and its [`CaptureBarrier`] opens it
/// again once the capture continues or is canceled, or the barrier is dropped. A clone that a
/// restore started gets the fence back closed, in its [`Restored`], and a capture request that
/// fails leaves it closed. The fence need not end the executions that run already: a template's
/// guest captures before it starts any. A preparation takes the fence, so a caller that keeps
/// its admission's state lends it a handle of that state, or a mutable reference to its fence,
/// which the trait's implementation for `&mut F` takes. Several handles may share one state, so
/// closing the fence acquires it: [`close`](Self::close) fails while the fence is closed, such as
/// by another preparation's barrier, and a preparation opens only a fence that it closed, so it
/// never opens one that another holds closed.
pub trait AdmissionFence {
    /// Closes the fence: once it returns, no execution starts until [`open`](Self::open). It
    /// fails while the fence is closed already, whoever closed it, and a failure leaves the fence
    /// as it was.
    fn close(&mut self) -> io::Result<()>;

    /// Opens the fence that [`close`](Self::close) closed, so that executions start again.
    fn open(&mut self);
}

impl<F: AdmissionFence + ?Sized> AdmissionFence for &mut F {
    fn close(&mut self) -> io::Result<()> {
        (**self).close()
    }

    fn open(&mut self) {
        (**self).open();
    }
}

/// The devices of a template's guest from which a capture must find nothing mounted, so that
/// each clone attaches its own.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Devices {
    /// The `distro` block device, which holds a layered root's image, named by its MMIO window,
    /// such as [`DISTRO_MMIO_BASE`](crate::protocol::root::DISTRO_MMIO_BASE). Nothing may be
    /// mounted from it or from a partition of it. The capture checks no size.
    pub distro: Device,
    /// The scratch block device, named by its MMIO window, such as
    /// [`SCRATCH_MMIO_BASE`](crate::protocol::root::SCRATCH_MMIO_BASE). Nothing may be mounted
    /// from it. The capture checks no size.
    pub scratch: Device,
    /// The virtio-fs tag of the host filesystem device, which the mount table shows as the source
    /// of the device's mounts. Nothing may be mounted from it.
    pub filesystem_tag: String,
}

/// The preparation of a template's guest for a capture, which the caller makes of its
/// workload-admission fence and the identities of the devices from which the snapshot must hold
/// no mount; see the [module](self).
pub struct CapturePreparation<F> {
    /// The caller's fence of workload admission.
    fence: F,
    /// The devices from which nothing may be mounted.
    devices: Devices,
}

impl<F: AdmissionFence> CapturePreparation<F> {
    /// Returns the preparation of a capture behind the caller's `fence`, of a guest from whose
    /// `devices` nothing may be mounted.
    pub fn new(fence: F, devices: Devices) -> Self {
        Self { fence, devices }
    }

    /// Prepares the guest for the capture, as the [module](self) describes: closes the fence,
    /// checks the mount table, waits for the time ABI's boot checks, runs its capture checks, and
    /// suppresses the stall detectors, then returns the barrier that holds them.
    ///
    /// The wait for the boot checks lasts 30 seconds at most. A capture check that fails reports
    /// a violation of the time ABI, which stops the VM unless the ABI is report-only, since the
    /// preparation enforces it even in [strict mode](self#strict-mode); in report-only mode, the
    /// preparation goes on, as `nvx-time pre-capture` does. A preparation that fails after it
    /// closed the fence opens it again, and leaves the detectors' settings as they were: when it
    /// suppressed some already, it restores their settings, and a failure to restore them is a
    /// [`CaptureStage::Release`] error. One after which the calling thread may still hold some of
    /// the permission for the time ABI's ports fails with [`TimeError::PortRelease`] instead,
    /// after which the thread must start neither a thread nor a process, which would inherit it.
    pub fn prepare(self) -> Result<CaptureBarrier<F>, CaptureError> {
        let system = LiveSystem {
            paths: Paths::default(),
            abi: live_abi(),
        };
        self.prepare_with(Box::new(system))
    }

    /// Implements [`prepare`](Self::prepare) on `system`.
    fn prepare_with(self, system: Box<dyn System>) -> Result<CaptureBarrier<F>, CaptureError> {
        let Self { mut fence, devices } = self;
        // A fence that failed to close, which another may hold closed, is never opened.
        fence
            .close()
            .map_err(|error| CaptureError::new(CaptureStage::Fence, CaptureCause::Io(error)))?;
        let mut barrier = CaptureBarrier {
            fence: Some(fence),
            suppressed: false,
            system,
        };
        check_mounts(barrier.system.as_mut(), &devices)
            .map_err(|cause| CaptureError::new(CaptureStage::Mounts, cause))?;
        if let Err(error) = barrier.system.pre_capture() {
            // The barrier restores a suppression that the time ABI holds after a failure, as
            // after a preparation that succeeded.
            barrier.suppressed = error.holds_suppression();
            let error = match error {
                PreCaptureError::BootChecks(error) => {
                    CaptureError::new(CaptureStage::BootChecks, CaptureCause::Time(error))
                }
                PreCaptureError::CaptureChecks(error)
                | PreCaptureError::PortRelease { error, .. } => {
                    CaptureError::new(CaptureStage::CaptureChecks, CaptureCause::Time(error))
                }
                PreCaptureError::Pending => {
                    CaptureError::new(CaptureStage::Suppression, CaptureCause::Pending)
                }
                PreCaptureError::Suppression(error) | PreCaptureError::Unrestored(error) => {
                    CaptureError::new(CaptureStage::Suppression, CaptureCause::Io(error.into()))
                }
            };
            return Err(barrier.fail(error));
        }
        barrier.suppressed = true;
        Ok(barrier)
    }
}

impl<F> fmt::Debug for CapturePreparation<F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapturePreparation")
            .field("devices", &self.devices)
            .finish_non_exhaustive()
    }
}

/// Checks, in the mount table that `system` reads, that nothing is mounted from `devices` or an
/// overlay, and fails with the first mount that is, or with why the table could not be checked.
fn check_mounts(system: &mut dyn System, devices: &Devices) -> Result<(), CaptureCause> {
    let table = system.mount_table().map_err(CaptureCause::Io)?;
    if table.len() > MOUNT_TABLE_LIMIT {
        return Err(CaptureCause::Io(io::Error::new(
            io::ErrorKind::FileTooLarge,
            format!("the mount table is longer than {MOUNT_TABLE_LIMIT} bytes"),
        )));
    }
    match mounts::first_blocking(system, devices, &table).map_err(CaptureCause::Io)? {
        Some(mounted) => Err(CaptureCause::Mounted(mounted)),
        None => Ok(()),
    }
}

/// The barrier of a prepared capture, which holds the caller's fence closed and the stall
/// detectors suppressed until the capture ends, or the barrier is canceled or dropped; see the
/// [module](self).
#[must_use = "dropping the barrier opens the fence and restores the stall detectors"]
pub struct CaptureBarrier<F: AdmissionFence> {
    /// The caller's fence of workload admission, which the barrier holds closed, until it opens
    /// it, or hands it over closed.
    fence: Option<F>,
    /// Whether the barrier suppressed the stall detectors, whose settings it restores.
    suppressed: bool,
    /// The kernel and the time ABI.
    system: Box<dyn System>,
}

impl<F: AdmissionFence> CaptureBarrier<F> {
    /// Requests the capture: reads the VM generation ID, then requests a capture through
    /// [`TimeAbi::capture`] with the platform tier's request, which keeps no scratch, in the
    /// in-memory mode, and returns how it ended.
    ///
    /// A VM that continues without a restore gets [`Captured::Continued`], once the barrier
    /// opened the fence and restored the detectors' settings; a failure to restore them is a
    /// [`CaptureStage::Release`] error, after which the caller ends the VM. A clone that a
    /// restore of the snapshot started gets [`Captured::Restored`], with the fence still closed
    /// and the detectors still suppressed: see the [module](self).
    ///
    /// A failure to read the VM generation ID, before the request, opens the fence and restores
    /// the detectors' settings, as [`cancel`](Self::cancel) does. A request that fails restores
    /// the detectors' settings, but leaves the fence closed, since it may fail in a clone whose
    /// restore it could not repair, where no execution may start: there, the time ABI, in
    /// [strict mode](self#strict-mode), fails it with the violation, [`TimeError::Violation`],
    /// rather than stopping the VM. Either way, the caller ends the VM, with the violation's
    /// status if it has one, and a failure to restore the settings is a [`CaptureStage::Release`]
    /// error, unless the calling thread may still hold some of the permission for the time ABI's
    /// ports, which [`TimeError::PortRelease`] reports instead.
    pub fn capture(mut self) -> Result<Captured<F>, CaptureError> {
        let generation_id = match self.system.generation_id() {
            Ok(generation_id) => generation_id,
            Err(error) => {
                let error =
                    CaptureError::new(CaptureStage::GenerationId, CaptureCause::Time(error));
                return Err(self.fail(error));
            }
        };
        // The restore holds its fresh entropy as its seed, which no file holds, until repair
        // takes it.
        let request = CaptureRequest::in_memory(ScratchPolicy::Fresh, generation_id);
        let outcome = match self.system.capture(request) {
            Ok(outcome) => outcome,
            Err(error) => {
                // The fence is dropped closed.
                self.fence = None;
                let error = CaptureError::new(CaptureStage::Capture, CaptureCause::Time(error));
                return Err(self.fail(error));
            }
        };
        match outcome {
            CaptureOutcome::Continued => {
                self.release()?;
                Ok(Captured::Continued)
            }
            CaptureOutcome::Restored(restore) => {
                // The time daemon restores the detectors' settings once repair finishes the
                // restore, and the caller keeps the fence closed through repair.
                self.suppressed = false;
                let fence = self
                    .fence
                    .take()
                    .expect("a barrier holds the fence until it releases it");
                Ok(Captured::Restored(Restored { restore, fence }))
            }
        }
    }

    /// Cancels the capture: opens the fence, then restores the stall detectors' settings. A
    /// failure to restore them is a [`CaptureStage::Release`] error, after which the caller ends
    /// the VM.
    pub fn cancel(mut self) -> Result<(), CaptureError> {
        self.release()
    }

    /// Opens the fence, then restores the stall detectors' settings, if the barrier holds them.
    fn release(&mut self) -> Result<(), CaptureError> {
        if let Some(mut fence) = self.fence.take() {
            fence.open();
        }
        if mem::take(&mut self.suppressed) {
            self.system.cancel_capture().map_err(|detail| {
                let detail = String::from_utf8_lossy(&detail).into_owned();
                CaptureError::new(
                    CaptureStage::Release,
                    CaptureCause::Io(io::Error::other(detail)),
                )
            })?;
        }
        Ok(())
    }

    /// Releases what the barrier holds after `error`, as [`release`](Self::release) does, and
    /// returns `error`, or the failure to restore the stall detectors' settings, unless `error`
    /// tells that the calling thread may still hold some of the permission for the time ABI's
    /// ports, which takes precedence.
    fn fail(&mut self, error: CaptureError) -> CaptureError {
        match self.release() {
            Err(release)
                if !matches!(error.cause, CaptureCause::Time(TimeError::PortRelease(_))) =>
            {
                release
            }
            _ => error,
        }
    }
}

impl<F: AdmissionFence> Drop for CaptureBarrier<F> {
    /// Opens the fence and restores the stall detectors' settings, as
    /// [`CaptureBarrier::cancel`] does, and ignores a failure.
    fn drop(&mut self) {
        let _ = self.release();
    }
}

impl<F: AdmissionFence> fmt::Debug for CaptureBarrier<F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CaptureBarrier")
            .field("fenced", &self.fence.is_some())
            .field("suppressed", &self.suppressed)
            .finish_non_exhaustive()
    }
}

/// How a capture that [`CaptureBarrier::capture`] requested ended, in the VM in which it
/// returned.
pub enum Captured<F> {
    /// The VM continued without a restore, as [`CaptureOutcome::Continued`] tells: no snapshot
    /// was taken, or the restore packet was of the recorded generation. The barrier opened the
    /// fence and restored the stall detectors' settings.
    Continued,
    /// A restore of the snapshot started the VM, a clone, whose wall clock the time ABI repaired.
    /// The fence stays closed, and the stall detectors stay suppressed.
    Restored(Restored<F>),
}

impl<F> fmt::Debug for Captured<F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Continued => formatter.write_str("Continued"),
            Self::Restored(restored) => formatter.debug_tuple("Restored").field(restored).finish(),
        }
    }
}

/// A clone's restore, which [`CaptureBarrier::capture`] returns for repair to take next, with
/// the caller's fence, which stays closed until the caller opens it.
///
/// No execution may start before the clone's repair acknowledged the restore, and repair never
/// opens the fence: the caller opens it once the clone may admit executions, such as once it is
/// activated. Dropping it leaves the fence closed.
pub struct Restored<F> {
    /// The restore that the time ABI repaired, as its restore packet describes it.
    restore: Restore,
    /// The caller's fence of workload admission, closed.
    fence: F,
}

impl<F> Restored<F> {
    /// Returns the restore that the time ABI repaired: its new VM generation ID, its targets, and
    /// whether the host gates its input until the guest acknowledges it.
    pub fn restore(&self) -> &Restore {
        &self.restore
    }

    /// Returns the restore that the time ABI repaired, which holds the restore's fresh entropy as
    /// its seed, for [`repair`](fn@repair) or a [`Repair`] to take, and the caller's fence, still
    /// closed.
    pub fn into_parts(self) -> (Restore, F) {
        (self.restore, self.fence)
    }
}

impl<F> fmt::Debug for Restored<F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Restored")
            .field("restore", &self.restore)
            .finish_non_exhaustive()
    }
}

/// A stage of a capture, which a [`CaptureError`] names.
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
    /// Restoring the stall detectors' settings, as the barrier releases them.
    Release,
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
            Self::Release => "restore the stall detectors",
        })
    }
}

/// Why a stage of a capture failed, which a [`CaptureError`] holds.
#[derive(Debug)]
#[non_exhaustive]
pub enum CaptureCause {
    /// The caller's fence, or a system call, failed with this error: a read of the mount table
    /// or of a device's entries in procfs and sysfs, which names the file, a mount table longer
    /// than [`MOUNT_TABLE_LIMIT`] or of another form than the kernel's, or a save or restore of
    /// the stall detectors' settings.
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
}

impl fmt::Display for CaptureCause {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => formatter.write_str(&sys::describe(error)),
            Self::Mounted(mounted) => mounted.fmt(formatter),
            Self::Pending => formatter.write_str("a capture is already pending"),
            Self::Time(error) => error.fmt(formatter),
        }
    }
}

/// Why a capture's preparation, request, or release failed: the stage that failed, and its cause.
///
/// Its `Display` form names both, such as `failed to check the mount table: /run/nvx/rootfs is
/// mounted from an overlay`.
#[derive(Debug)]
pub struct CaptureError {
    /// The stage that failed.
    stage: CaptureStage,
    /// Why it failed.
    cause: CaptureCause,
}

impl CaptureError {
    /// Returns the failure of `stage`, because of `cause`.
    fn new(stage: CaptureStage, cause: CaptureCause) -> Self {
        Self { stage, cause }
    }

    /// Returns the stage that failed.
    pub fn stage(&self) -> CaptureStage {
        self.stage
    }

    /// Returns why it failed.
    pub fn cause(&self) -> &CaptureCause {
        &self.cause
    }

    /// Returns why it failed.
    pub fn into_cause(self) -> CaptureCause {
        self.cause
    }
}

impl fmt::Display for CaptureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "failed to {}: {}", self.stage, self.cause)
    }
}

impl std::error::Error for CaptureError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.cause {
            CaptureCause::Io(error) => Some(error),
            CaptureCause::Time(error) => Some(error),
            CaptureCause::Mounted(_) | CaptureCause::Pending => None,
        }
    }
}

/// What a capture reads of the kernel and asks of the time ABI, which the tests' fake replaces.
trait System: Send {
    /// Reads the mount table of the calling process's mount namespace, at most one byte more than
    /// [`MOUNT_TABLE_LIMIT`] of it.
    fn mount_table(&mut self) -> io::Result<Vec<u8>>;

    /// Returns the canonical sysfs directories of the virtio MMIO windows at `mmio` whose devices
    /// the kernel added, below which it creates their block devices and partitions.
    fn windows(&mut self, mmio: u64) -> io::Result<Vec<PathBuf>>;

    /// Returns the canonical sysfs directory of the block device whose number is `device`.
    fn block_device(&mut self, device: DeviceNumber) -> io::Result<PathBuf>;

    /// Waits for the time ABI's boot checks, runs its capture checks, and suppresses the stall
    /// detectors, as [`TimeAbi::pre_capture`] does.
    fn pre_capture(&mut self) -> Result<(), PreCaptureError>;

    /// Restores the stall detectors' settings, as [`TimeAbi::cancel_capture`] does.
    fn cancel_capture(&mut self) -> Result<(), Vec<u8>>;

    /// Reads the VM generation ID, as [`TimeAbi::generation_id`] does.
    fn generation_id(&mut self) -> Result<GenerationId, TimeError>;

    /// Requests a capture, as [`TimeAbi::capture`] does.
    fn capture(&mut self, request: CaptureRequest) -> Result<CaptureOutcome, TimeError>;
}

/// The guest's kernel, at the paths of its interfaces, and its time ABI.
struct LiveSystem {
    /// The directories of the kernel's interfaces.
    paths: Paths,
    /// The time ABI of [`live_abi`], whose preparation of a capture enforces the ABI all the same.
    abi: TimeAbi,
}

/// Returns the time ABI of the capture and of the repair: in strict mode, unless the kernel
/// command line makes it report-only.
fn live_abi() -> TimeAbi {
    TimeAbi::new().strict()
}

impl System for LiveSystem {
    fn mount_table(&mut self) -> io::Result<Vec<u8>> {
        let path = self.paths.proc.join(MOUNT_TABLE);
        let mut table = Vec::new();
        File::open(&path)
            .and_then(|file| {
                file.take(MOUNT_TABLE_LIMIT as u64 + 1)
                    .read_to_end(&mut table)
            })
            .map_err(|error| at(&path, &error))?;
        Ok(table)
    }

    fn windows(&mut self, mmio: u64) -> io::Result<Vec<PathBuf>> {
        let windows = root::window_directories(&self.paths, mmio)
            .map_err(|error| at(&self.paths.proc.join("iomem"), &error))?;
        let mut directories = Vec::with_capacity(windows.len());
        for window in windows {
            match fs::canonicalize(&window) {
                Ok(directory) => directories.push(directory),
                // The kernel lists the window before it adds the window's device, from which
                // nothing is mounted yet.
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(at(&window, &error)),
            }
        }
        Ok(directories)
    }

    fn block_device(&mut self, device: DeviceNumber) -> io::Result<PathBuf> {
        let path = self.paths.sys.join(BLOCK_DEVICES).join(device.to_string());
        fs::canonicalize(&path).map_err(|error| at(&path, &error))
    }

    fn pre_capture(&mut self) -> Result<(), PreCaptureError> {
        self.abi.pre_capture()
    }

    fn cancel_capture(&mut self) -> Result<(), Vec<u8>> {
        self.abi.cancel_capture()
    }

    fn generation_id(&mut self) -> Result<GenerationId, TimeError> {
        self.abi.generation_id()
    }

    fn capture(&mut self, request: CaptureRequest) -> Result<CaptureOutcome, TimeError> {
        self.abi.capture(request)
    }
}

/// Returns `error`, of an operation on `path`, as an error of the same kind whose text names the
/// path, such as `/proc/self/mountinfo: Permission denied`.
fn at(path: &Path, error: &io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("{}: {}", path.display(), sys::describe(error)),
    )
}
