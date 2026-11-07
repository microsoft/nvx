//! Tests of a capture's preparation, request, and release, with a fake fence and a fake system,
//! whose mount table, devices, and time ABI each test chooses, which record their calls in one
//! log, and of the live system's lookups in a temporary directory.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use super::mounts::{self, DeviceNumber};
use super::*;
use crate::guest::reseed::Seed;
use crate::guest::sys::{Errno, PortError};

/// The base address of the distro device's MMIO window.
const DISTRO: u64 = 0xd000_3000;
/// The base address of the scratch device's MMIO window.
const SCRATCH: u64 = 0xd000_6000;
/// The virtio-fs tag of the host filesystem device.
const TAG: &str = "microvm";
/// The sysfs directory of the distro device's MMIO window.
const DISTRO_WINDOW: &str = "/sys/devices/virtio-mmio-cmdline/virtio-mmio.0";
/// The sysfs directory of the scratch device's MMIO window.
const SCRATCH_WINDOW: &str = "/sys/devices/virtio-mmio-cmdline/virtio-mmio.1";
/// The VM generation ID of the template.
const GENERATION: GenerationId = GenerationId::from_bytes([7; GenerationId::LENGTH]);
/// The VM generation ID of a clone that a restore started.
const RESTORED: GenerationId = GenerationId::from_bytes([8; GenerationId::LENGTH]);

/// The mount table of a template's guest before its root's assembly, which blocks no capture.
const TEMPLATE_TABLE: &str = "\
1 1 0:2 / / rw - rootfs rootfs rw
20 1 0:20 / /proc rw,nosuid,nodev,noexec,relatime shared:2 - proc none rw
21 1 0:21 / /sys rw,nosuid,nodev,noexec,relatime shared:3 - sysfs none rw
22 1 0:5 / /dev rw,nosuid,relatime shared:4 - devtmpfs none rw,size=65536k,mode=755
23 1 0:22 / /run rw,nosuid,nodev,relatime shared:5 - tmpfs tmpfs rw,mode=755
24 1 0:23 / /tmp rw,relatime shared:6 - tmpfs none rw
25 21 0:24 / /sys/fs/cgroup rw,nosuid,nodev,noexec,relatime shared:7 - cgroup2 none rw
";

/// A call of a test's fence or system.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    /// The fence closed.
    Close,
    /// The fence opened.
    Open,
    /// The mount table was read.
    MountTable,
    /// The windows at an MMIO base were looked up.
    Windows(u64),
    /// A block device was looked up by its number.
    BlockDevice(DeviceNumber),
    /// The time ABI prepared the capture.
    PreCapture,
    /// The stall detectors' settings were restored.
    CancelCapture,
    /// The VM generation ID was read.
    GenerationId,
    /// A capture was requested.
    Capture(CaptureRequest),
}

/// The log of the calls of a test's fence and system, which they share.
#[derive(Clone, Default)]
struct Calls(Arc<Mutex<Vec<Call>>>);

impl Calls {
    /// Records `call`.
    fn push(&self, call: Call) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(call);
    }

    /// Returns the calls recorded since the last take, in order.
    fn take(&self) -> Vec<Call> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// A fence that records when it closes and opens, and that fails to close with an error that a
/// test chooses, or while it is closed, whichever of its handles closed it.
struct FakeFence {
    /// The log.
    calls: Calls,
    /// The error number with which the fence fails to close, if any.
    failure: Option<Errno>,
    /// Whether the fence is closed, which its handles share.
    closed: Arc<AtomicBool>,
}

impl FakeFence {
    /// Returns another handle of the fence, which shares its state and its log.
    fn handle(&self) -> Self {
        Self {
            calls: self.calls.clone(),
            failure: None,
            closed: Arc::clone(&self.closed),
        }
    }

    /// Returns whether the fence is closed.
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

impl AdmissionFence for FakeFence {
    fn close(&mut self) -> io::Result<()> {
        if let Some(errno) = self.failure {
            return Err(errno.into());
        }
        if self.closed.swap(true, Ordering::SeqCst) {
            return Err(Errno::EBUSY.into());
        }
        self.calls.push(Call::Close);
        Ok(())
    }

    fn open(&mut self) {
        assert!(
            self.closed.swap(false, Ordering::SeqCst),
            "the fence opened while open"
        );
        self.calls.push(Call::Open);
    }
}

/// How the fake time ABI's preparation of a capture ends.
#[derive(Clone, Copy, Debug)]
enum Preparation {
    /// It suppresses the stall detectors.
    Succeeds,
    /// The boot checks are still pending.
    BootChecks,
    /// The capture checks cannot run.
    CaptureChecks,
    /// Another capture is pending.
    Pending,
    /// The detectors' settings cannot be saved, with this error number.
    Suppression(Errno),
    /// A detector cannot be suppressed, nor the others restored, with this error number, so
    /// the time ABI holds their suppression.
    Unrestored(Errno),
    /// The thread may still hold some of the permission for the ports, with this error number,
    /// after a preparation that held the suppression or not.
    PortRelease(Errno, bool),
}

/// How the fake capture ends.
#[derive(Clone, Copy, Debug)]
enum Outcome {
    /// The VM continues without a restore.
    Continued,
    /// A restore started the VM, a clone.
    Restored,
    /// The request fails.
    Fails,
    /// The request fails, and the thread may still hold some of the permission for the ports.
    PortRelease,
}

/// A system that records its calls, with a mount table, devices, and a time ABI that a test
/// chooses: by default, the template's table, the distro's and the scratch's windows, a distro
/// device with partitions, a scratch device, and two other devices, and a time ABI that
/// prepares, reads the VM generation ID, and continues without a restore.
struct FakeSystem {
    /// The log.
    calls: Calls,
    /// The mount table, or the error number of its read.
    table: Result<Vec<u8>, Errno>,
    /// The sysfs directories of the windows at each MMIO base.
    windows: HashMap<u64, Vec<PathBuf>>,
    /// The sysfs directory of each block device.
    devices: HashMap<DeviceNumber, PathBuf>,
    /// How the time ABI's preparation ends.
    preparation: Preparation,
    /// Whether the detectors' settings cannot be restored.
    restoration_fails: bool,
    /// Whether the VM generation ID cannot be read.
    generation_fails: bool,
    /// How the capture ends.
    outcome: Outcome,
}

impl FakeSystem {
    /// Returns the default system, which records its calls in `calls`.
    fn new(calls: &Calls) -> Self {
        let block = |window: &str, path: &str| PathBuf::from(format!("{window}/{path}"));
        Self {
            calls: calls.clone(),
            table: Ok(TEMPLATE_TABLE.as_bytes().to_vec()),
            windows: HashMap::from([
                (DISTRO, vec![PathBuf::from(DISTRO_WINDOW)]),
                (SCRATCH, vec![PathBuf::from(SCRATCH_WINDOW)]),
            ]),
            devices: HashMap::from([
                (number(254, 0), block(DISTRO_WINDOW, "virtio0/block/vda")),
                (
                    number(254, 2),
                    block(DISTRO_WINDOW, "virtio0/block/vda/vda2"),
                ),
                // The kernel numbers partitions after the 15th from the extended major.
                (
                    number(259, 0),
                    block(DISTRO_WINDOW, "virtio0/block/vda/vda17"),
                ),
                (number(254, 16), block(SCRATCH_WINDOW, "virtio1/block/vdb")),
                (
                    number(254, 32),
                    PathBuf::from(
                        "/sys/devices/virtio-mmio-cmdline/virtio-mmio.2/virtio2/block/vdc",
                    ),
                ),
                // The window's name starts with the scratch window's, but is another.
                (
                    number(254, 48),
                    PathBuf::from(
                        "/sys/devices/virtio-mmio-cmdline/virtio-mmio.10/virtio10/block/vdd",
                    ),
                ),
            ]),
            preparation: Preparation::Succeeds,
            restoration_fails: false,
            generation_fails: false,
            outcome: Outcome::Continued,
        }
    }

    /// Returns the system with the template's mount table and `lines` after it.
    fn with_lines(mut self, lines: &str) -> Self {
        self.table = Ok(format!("{TEMPLATE_TABLE}{lines}").into_bytes());
        self
    }
}

impl System for FakeSystem {
    fn mount_table(&mut self) -> io::Result<Vec<u8>> {
        self.calls.push(Call::MountTable);
        self.table.clone().map_err(io::Error::from)
    }

    fn windows(&mut self, mmio: u64) -> io::Result<Vec<PathBuf>> {
        self.calls.push(Call::Windows(mmio));
        Ok(self.windows.get(&mmio).cloned().unwrap_or_default())
    }

    fn block_device(&mut self, device: DeviceNumber) -> io::Result<PathBuf> {
        self.calls.push(Call::BlockDevice(device));
        self.devices
            .get(&device)
            .cloned()
            .ok_or_else(|| Errno::ENOENT.into())
    }

    fn pre_capture(&mut self) -> Result<(), PreCaptureError> {
        self.calls.push(Call::PreCapture);
        match self.preparation {
            Preparation::Succeeds => Ok(()),
            Preparation::BootChecks => Err(PreCaptureError::BootChecks(TimeError::PortAccess(
                Errno::EPERM.into(),
            ))),
            Preparation::CaptureChecks => {
                Err(PreCaptureError::CaptureChecks(TimeError::StateUnavailable))
            }
            Preparation::Pending => Err(PreCaptureError::Pending),
            Preparation::Suppression(errno) => Err(PreCaptureError::Suppression(errno)),
            Preparation::Unrestored(errno) => Err(PreCaptureError::Unrestored(errno)),
            Preparation::PortRelease(errno, suppressed) => Err(PreCaptureError::PortRelease {
                error: PortError::Revoke(errno).into(),
                suppressed,
            }),
        }
    }

    fn cancel_capture(&mut self) -> Result<(), Vec<u8>> {
        self.calls.push(Call::CancelCapture);
        if self.restoration_fails {
            Err(b"read /run/nvx/time-suppression: No such file or directory".to_vec())
        } else {
            Ok(())
        }
    }

    fn generation_id(&mut self) -> Result<GenerationId, TimeError> {
        self.calls.push(Call::GenerationId);
        if self.generation_fails {
            Err(TimeError::GenerationIdUnavailable)
        } else {
            Ok(GENERATION)
        }
    }

    fn capture(&mut self, request: CaptureRequest) -> Result<CaptureOutcome, TimeError> {
        self.calls.push(Call::Capture(request));
        match self.outcome {
            Outcome::Continued => Ok(CaptureOutcome::Continued),
            Outcome::Restored => Ok(CaptureOutcome::Restored(
                Restore::to(RESTORED).holding(seed()),
            )),
            Outcome::Fails => Err(TimeError::StateUnavailable),
            Outcome::PortRelease => Err(PortError::Revoke(Errno::ENOMEM).into()),
        }
    }
}

/// Returns the device number `major:minor`.
fn number(major: u32, minor: u32) -> DeviceNumber {
    DeviceNumber { major, minor }
}

/// Returns the template's devices: the distro and the scratch devices at their windows, and the
/// host filesystem device's tag.
fn devices() -> Devices {
    Devices {
        distro: Device {
            mmio: DISTRO,
            sectors: None,
        },
        scratch: Device {
            mmio: SCRATCH,
            sectors: Some(2048),
        },
        filesystem_tag: TAG.to_owned(),
    }
}

/// Returns an open fence that records its calls in `calls`.
fn fence(calls: &Calls) -> FakeFence {
    FakeFence {
        calls: calls.clone(),
        failure: None,
        closed: Arc::default(),
    }
}

/// Prepares a capture of the template's devices behind `fence`, on `system`.
fn prepare(
    fence: FakeFence,
    system: FakeSystem,
) -> Result<CaptureBarrier<FakeFence>, CaptureError> {
    CapturePreparation::new(fence, devices()).prepare_with(Box::new(system))
}

/// Returns a barrier that `system` prepared behind a fence, which record their calls in `calls`,
/// with the preparation's calls taken.
fn prepared(calls: &Calls, system: FakeSystem) -> CaptureBarrier<FakeFence> {
    let barrier = prepare(fence(calls), system).unwrap();
    assert_eq!(calls.take(), preparation_calls());
    barrier
}

/// Returns the calls of a preparation of the template's table that succeeds.
fn preparation_calls() -> Vec<Call> {
    vec![
        Call::Close,
        Call::MountTable,
        Call::Windows(DISTRO),
        Call::Windows(SCRATCH),
        Call::PreCapture,
    ]
}

/// Returns the platform tier's request of a capture of the template, in the in-memory mode.
fn platform_request() -> CaptureRequest {
    CaptureRequest::in_memory(ScratchPolicy::Fresh, GENERATION)
}

/// Returns the seed of the clone, whose bytes start with its new VM generation ID.
fn seed() -> Seed {
    let mut bytes = [0x5a; Seed::LENGTH];
    bytes[..GenerationId::LENGTH].copy_from_slice(&RESTORED.to_bytes());
    Seed::from_bytes(bytes)
}

#[test]
fn the_fence_closes_before_the_checks_and_the_detectors_are_suppressed_before_the_capture() {
    let calls = Calls::default();
    let barrier = prepare(fence(&calls), FakeSystem::new(&calls)).unwrap();
    // The fence first, then the mount table, then the time ABI, which suppresses the detectors.
    assert_eq!(calls.take(), preparation_calls());
    assert!(matches!(barrier.capture().unwrap(), Captured::Continued));
    // The capture's request follows the suppression. A VM that continues opens the fence, then
    // restores the detectors' settings.
    assert_eq!(
        calls.take(),
        [
            Call::GenerationId,
            Call::Capture(platform_request()),
            Call::Open,
            Call::CancelCapture,
        ]
    );
}

#[test]
fn each_mount_from_the_template_s_devices_blocks_a_capture() {
    let cases = [
        (
            "30 23 254:0 / /run/nvx/layers/distro ro,relatime shared:8 - erofs /dev/vda ro",
            Origin::Distro,
            "/run/nvx/layers/distro",
        ),
        // A GPT disk's layers, in its partitions.
        (
            "30 23 254:2 / /run/nvx/layers/p2 ro,nosuid,nodev,relatime - ext4 /dev/vda2 ro",
            Origin::Distro,
            "/run/nvx/layers/p2",
        ),
        (
            "30 23 259:0 / /run/nvx/layers/p17 ro,nosuid,nodev,relatime - ext4 /dev/vda17 ro",
            Origin::Distro,
            "/run/nvx/layers/p17",
        ),
        // A bind mount of a directory of the distro's file system.
        (
            "30 23 254:0 /etc /mnt/etc ro,relatime - erofs /dev/vda ro",
            Origin::Distro,
            "/mnt/etc",
        ),
        (
            "30 23 254:16 / /run/nvx/scratch rw,nosuid,nodev,relatime - ext4 /dev/vdb rw",
            Origin::Scratch,
            "/run/nvx/scratch",
        ),
        (
            "30 23 0:30 / /run/nvx/rootfs rw,nosuid,nodev,relatime - overlay overlay \
             rw,lowerdir=/run/nvx/layers/distro,upperdir=/run/nvx/tmpfs/upper,\
             workdir=/run/nvx/tmpfs/work",
            Origin::Overlay,
            "/run/nvx/rootfs",
        ),
        (
            "30 23 0:31 / /run/nvx/hostfs/root ro,nosuid,nodev,relatime - virtiofs microvm ro",
            Origin::Filesystem,
            "/run/nvx/hostfs/root",
        ),
        // A bind mount of a child of the export, at a mount point that the kernel escapes.
        (
            "30 23 0:31 /src /work\\040tree rw,nosuid,nodev,relatime - virtiofs microvm rw",
            Origin::Filesystem,
            "/work tree",
        ),
    ];
    for (line, origin, mount_point) in cases {
        let calls = Calls::default();
        let system = FakeSystem::new(&calls).with_lines(&format!("{line}\n"));
        let error = prepare(fence(&calls), system).unwrap_err();
        assert_eq!(error.stage(), CaptureStage::Mounts, "{line}");
        let CaptureCause::Mounted(mounted) = error.cause() else {
            panic!("{error:?}");
        };
        assert_eq!(mounted.origin(), origin, "{line}");
        assert_eq!(mounted.mount_point(), Path::new(mount_point));
        assert_eq!(
            error.to_string(),
            format!("failed to check the mount table: {mount_point} is mounted from {origin}")
        );
        // The time ABI prepares nothing, and the fence opens again.
        let calls = calls.take();
        assert_eq!(calls.first(), Some(&Call::Close));
        assert_eq!(calls.last(), Some(&Call::Open));
        assert!(!calls.contains(&Call::PreCapture), "{calls:?}");
        assert!(!calls.contains(&Call::CancelCapture), "{calls:?}");
    }
}

#[test]
fn the_first_blocking_mount_of_the_table_is_reported() {
    let calls = Calls::default();
    let system = FakeSystem::new(&calls).with_lines(
        "30 23 0:30 / /run/nvx/rootfs rw - overlay overlay rw\n\
         31 23 254:0 / /run/nvx/layers/distro ro - erofs /dev/vda ro\n",
    );
    let error = prepare(fence(&calls), system).unwrap_err();
    assert_eq!(
        error.to_string(),
        "failed to check the mount table: /run/nvx/rootfs is mounted from an overlay"
    );
    assert_eq!(
        calls.take(),
        [
            Call::Close,
            Call::MountTable,
            Call::Windows(DISTRO),
            Call::Windows(SCRATCH),
            Call::Open,
        ]
    );
}

#[test]
fn mounts_from_other_devices_and_tags_block_no_capture() {
    let calls = Calls::default();
    // Another tag's export, a tmpfs whose source names the tag or an overlay, a file system on
    // another block device, and one whose source names the distro's node but whose device is
    // another's, below a window whose name only starts with the scratch's.
    let system = FakeSystem::new(&calls).with_lines(
        "30 23 0:31 / /mnt/other ro - virtiofs other ro\n\
         31 23 0:32 / /mnt/tag rw - tmpfs microvm rw\n\
         32 23 0:33 / /mnt/overlay rw - tmpfs overlay rw\n\
         33 23 254:32 / /mnt/vdc rw - ext4 /dev/vdc rw\n\
         34 23 254:48 / /mnt/vdd rw - ext4 /dev/vda rw\n",
    );
    let barrier = prepare(fence(&calls), system).unwrap();
    assert_eq!(
        calls.take(),
        [
            Call::Close,
            Call::MountTable,
            Call::Windows(DISTRO),
            Call::Windows(SCRATCH),
            Call::BlockDevice(number(254, 32)),
            Call::BlockDevice(number(254, 48)),
            Call::PreCapture,
        ]
    );
    barrier.cancel().unwrap();
    assert_eq!(calls.take(), [Call::Open, Call::CancelCapture]);
}

#[test]
fn a_device_without_a_window_has_nothing_mounted_from_it() {
    let calls = Calls::default();
    let mut system =
        FakeSystem::new(&calls).with_lines("30 23 254:32 / /mnt/vdc rw - ext4 /dev/vdc rw\n");
    // The kernel lists no window at the scratch's base, as in a VM without a scratch device.
    system.windows.remove(&SCRATCH);
    system.devices.remove(&number(254, 16));
    let barrier = prepare(fence(&calls), system).unwrap();
    assert!(calls.take().contains(&Call::PreCapture));
    drop(barrier);
    assert_eq!(calls.take(), [Call::Open, Call::CancelCapture]);
}

#[test]
fn a_mount_table_that_cannot_be_checked_blocks_a_capture() {
    let lines = |lines: &str| Ok(format!("{TEMPLATE_TABLE}{lines}").into_bytes());
    let cases = [
        (
            Err(Errno::EACCES),
            io::ErrorKind::PermissionDenied,
            Errno::EACCES.desc().to_owned(),
        ),
        (
            Ok(vec![b'\n'; MOUNT_TABLE_LIMIT + 1]),
            io::ErrorKind::FileTooLarge,
            format!("the mount table is longer than {MOUNT_TABLE_LIMIT} bytes"),
        ),
        (
            lines("30 23\n"),
            io::ErrorKind::InvalidData,
            "line 8 of the mount table is malformed".to_owned(),
        ),
        // A file system on a block device that sysfs does not show.
        (
            lines("30 23 8:0 / /mnt rw - ext4 /dev/sda rw\n"),
            io::ErrorKind::NotFound,
            Errno::ENOENT.desc().to_owned(),
        ),
    ];
    for (table, kind, text) in cases {
        let calls = Calls::default();
        let mut system = FakeSystem::new(&calls);
        system.table = table;
        let error = prepare(fence(&calls), system).unwrap_err();
        assert_eq!(error.stage(), CaptureStage::Mounts);
        let CaptureCause::Io(cause) = error.cause() else {
            panic!("{error:?}");
        };
        assert_eq!(cause.kind(), kind, "{error}");
        assert_eq!(
            error.to_string(),
            format!("failed to check the mount table: {text}")
        );
        let calls = calls.take();
        assert_eq!(calls.last(), Some(&Call::Open));
        assert!(!calls.contains(&Call::PreCapture), "{calls:?}");
    }
    // A table of the limit's length is read whole.
    let calls = Calls::default();
    let mut system = FakeSystem::new(&calls);
    system.table = Ok(vec![b'\n'; MOUNT_TABLE_LIMIT]);
    prepare(fence(&calls), system).unwrap().cancel().unwrap();
}

#[test]
fn a_fence_that_fails_to_close_stops_the_preparation() {
    let calls = Calls::default();
    let fence = FakeFence {
        failure: Some(Errno::EIO),
        ..fence(&calls)
    };
    let error = prepare(fence, FakeSystem::new(&calls)).unwrap_err();
    assert_eq!(error.stage(), CaptureStage::Fence);
    let CaptureCause::Io(cause) = error.cause() else {
        panic!("{error:?}");
    };
    assert_eq!(cause.raw_os_error(), Some(Errno::EIO as i32));
    assert_eq!(
        error.to_string(),
        format!("failed to fence workload admission: {}", Errno::EIO.desc())
    );
    // Nothing else runs, and the fence that did not close does not open.
    assert_eq!(calls.take(), Vec::<Call>::new());
}

#[test]
fn a_preparation_behind_a_fence_that_another_holds_closed_fails_and_leaves_it_closed() {
    let calls = Calls::default();
    let gate = fence(&calls);
    let barrier = prepare(gate.handle(), FakeSystem::new(&calls)).unwrap();
    assert_eq!(calls.take(), preparation_calls());
    // A second preparation behind the same gate, whose time ABI would find the first one's
    // suppression pending, cannot close the fence, so it never opens it.
    let mut system = FakeSystem::new(&calls);
    system.preparation = Preparation::Pending;
    let error = prepare(gate.handle(), system).unwrap_err();
    assert_eq!(error.stage(), CaptureStage::Fence);
    let CaptureCause::Io(cause) = error.cause() else {
        panic!("{error:?}");
    };
    assert_eq!(cause.raw_os_error(), Some(Errno::EBUSY as i32));
    assert_eq!(calls.take(), Vec::<Call>::new());
    assert!(gate.is_closed());

    // The first barrier holds the fence closed until it opens it, after which the next
    // preparation closes it again.
    barrier.cancel().unwrap();
    assert_eq!(calls.take(), [Call::Open, Call::CancelCapture]);
    assert!(!gate.is_closed());
    let barrier = prepare(gate.handle(), FakeSystem::new(&calls)).unwrap();
    assert_eq!(calls.take(), preparation_calls());
    drop(barrier);
    assert_eq!(calls.take(), [Call::Open, Call::CancelCapture]);
    assert!(!gate.is_closed());
}

#[test]
fn a_failed_time_preparation_names_its_stage_and_opens_the_fence() {
    // Each failure, its stage, and whether the time ABI holds the detectors' suppression.
    let cases = [
        (Preparation::BootChecks, CaptureStage::BootChecks, false),
        (
            Preparation::CaptureChecks,
            CaptureStage::CaptureChecks,
            false,
        ),
        (Preparation::Pending, CaptureStage::Suppression, false),
        (
            Preparation::Suppression(Errno::EROFS),
            CaptureStage::Suppression,
            false,
        ),
        (
            Preparation::Unrestored(Errno::EIO),
            CaptureStage::Suppression,
            true,
        ),
        (
            Preparation::PortRelease(Errno::ENOMEM, false),
            CaptureStage::CaptureChecks,
            false,
        ),
        (
            Preparation::PortRelease(Errno::ENOMEM, true),
            CaptureStage::CaptureChecks,
            true,
        ),
    ];
    for (failure, stage, held) in cases {
        let calls = Calls::default();
        let mut system = FakeSystem::new(&calls);
        system.preparation = failure;
        let error = prepare(fence(&calls), system).unwrap_err();
        assert_eq!(error.stage(), stage, "{failure:?}");
        match (failure, error.cause()) {
            (Preparation::BootChecks | Preparation::CaptureChecks, CaptureCause::Time(_))
            | (Preparation::Pending, CaptureCause::Pending) => {}
            (
                Preparation::Suppression(errno) | Preparation::Unrestored(errno),
                CaptureCause::Io(cause),
            ) if cause.raw_os_error() == Some(errno as i32) => {}
            (Preparation::PortRelease(..), CaptureCause::Time(TimeError::PortRelease(_))) => {
                assert_eq!(
                    error.to_string(),
                    format!(
                        "failed to run the time ABI's capture checks: ioperm revoke: {}",
                        Errno::ENOMEM.desc()
                    )
                );
            }
            _ => panic!("{failure:?}: {error:?}"),
        }
        // The fence opens, and the barrier restores the detectors' settings that the time ABI
        // holds, or nothing.
        let mut expected = preparation_calls();
        expected.push(Call::Open);
        if held {
            expected.push(Call::CancelCapture);
        }
        assert_eq!(calls.take(), expected, "{failure:?}");
    }
}

#[test]
fn a_held_suppression_that_a_failed_preparation_cannot_restore_is_a_release_error() {
    let calls = Calls::default();
    let mut system = FakeSystem::new(&calls);
    system.preparation = Preparation::Unrestored(Errno::EIO);
    system.restoration_fails = true;
    let error = prepare(fence(&calls), system).unwrap_err();
    assert_eq!(error.stage(), CaptureStage::Release);
    assert_eq!(
        error.to_string(),
        "failed to restore the stall detectors: read /run/nvx/time-suppression: No such file or \
         directory"
    );
    let mut expected = preparation_calls();
    expected.extend([Call::Open, Call::CancelCapture]);
    assert_eq!(calls.take(), expected);
}

#[test]
fn a_port_permission_that_the_thread_may_hold_outranks_a_failed_restore() {
    let calls = Calls::default();
    let mut system = FakeSystem::new(&calls);
    system.preparation = Preparation::PortRelease(Errno::ENOMEM, true);
    system.restoration_fails = true;
    let error = prepare(fence(&calls), system).unwrap_err();
    assert_eq!(error.stage(), CaptureStage::CaptureChecks);
    assert!(
        matches!(error.cause(), CaptureCause::Time(TimeError::PortRelease(_))),
        "{error:?}"
    );
    // The barrier still tried to restore the detectors' settings.
    let mut expected = preparation_calls();
    expected.extend([Call::Open, Call::CancelCapture]);
    assert_eq!(calls.take(), expected);
}

#[test]
fn cancel_and_drop_open_the_fence_then_restore_the_detectors() {
    let calls = Calls::default();
    prepared(&calls, FakeSystem::new(&calls)).cancel().unwrap();
    assert_eq!(calls.take(), [Call::Open, Call::CancelCapture]);

    drop(prepared(&calls, FakeSystem::new(&calls)));
    assert_eq!(calls.take(), [Call::Open, Call::CancelCapture]);
}

#[test]
fn a_failed_restoration_of_the_detectors_is_reported_after_the_fence_opens() {
    let failing = |calls: &Calls| {
        let mut system = FakeSystem::new(calls);
        system.restoration_fails = true;
        system
    };
    let text = "failed to restore the stall detectors: read /run/nvx/time-suppression: No such file \
                or directory";
    let calls = Calls::default();
    let error = prepared(&calls, failing(&calls)).cancel().unwrap_err();
    assert_eq!(error.stage(), CaptureStage::Release);
    assert_eq!(error.to_string(), text);
    // The barrier releases once: dropping it after the failure releases nothing more.
    assert_eq!(calls.take(), [Call::Open, Call::CancelCapture]);

    let error = prepared(&calls, failing(&calls)).capture().unwrap_err();
    assert_eq!(error.stage(), CaptureStage::Release);
    assert_eq!(error.to_string(), text);
    assert_eq!(
        calls.take(),
        [
            Call::GenerationId,
            Call::Capture(platform_request()),
            Call::Open,
            Call::CancelCapture,
        ]
    );

    drop(prepared(&calls, failing(&calls)));
    assert_eq!(calls.take(), [Call::Open, Call::CancelCapture]);
}

#[test]
fn a_restored_clone_gets_the_fence_closed_with_the_detectors_suppressed() {
    let calls = Calls::default();
    let restored = |calls: &Calls| {
        let mut system = FakeSystem::new(calls);
        system.outcome = Outcome::Restored;
        let Captured::Restored(restored) = prepared(calls, system).capture().unwrap() else {
            panic!("the clone continued");
        };
        restored
    };
    let clone = restored(&calls);
    // Nothing opens the fence before repair, and the time daemon restores the detectors'
    // settings once repair finishes the restore.
    assert_eq!(
        calls.take(),
        [Call::GenerationId, Call::Capture(platform_request())]
    );
    assert_eq!(clone.restore().generation_id(), RESTORED);
    let (mut restore, mut fence) = clone.into_parts();
    assert_eq!(
        format!("{restore:?}"),
        format!("{:?}", Restore::to(RESTORED).holding(seed()))
    );
    // The restore holds the clone's seed, for repair to take.
    assert_eq!(
        restore.take_seed().map(|seed| seed.generation_id()),
        Some(RESTORED)
    );
    assert_eq!(calls.take(), Vec::<Call>::new());
    // A preparation behind another handle of the fence cannot close it, so it opens nothing.
    let error = prepare(fence.handle(), FakeSystem::new(&calls)).unwrap_err();
    assert_eq!(error.stage(), CaptureStage::Fence);
    assert_eq!(calls.take(), Vec::<Call>::new());
    // The caller opens the fence once the clone may admit executions, after its repair.
    fence.open();
    assert_eq!(calls.take(), [Call::Open]);

    // A clone that is dropped leaves the fence closed.
    drop(restored(&calls));
    assert_eq!(
        calls.take(),
        [Call::GenerationId, Call::Capture(platform_request())]
    );
}

#[test]
fn a_failed_capture_restores_the_detectors_and_opens_the_fence_only_before_its_request() {
    let calls = Calls::default();
    let mut system = FakeSystem::new(&calls);
    system.generation_fails = true;
    let error = prepared(&calls, system).capture().unwrap_err();
    assert_eq!(error.stage(), CaptureStage::GenerationId);
    assert!(
        matches!(
            error.cause(),
            CaptureCause::Time(TimeError::GenerationIdUnavailable)
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "failed to read the VM generation ID: the VM generation ID is unavailable"
    );
    assert_eq!(
        calls.take(),
        [Call::GenerationId, Call::Open, Call::CancelCapture]
    );

    // A request that fails may fail in a clone that its restore left unrepaired, so the fence
    // stays closed.
    let mut system = FakeSystem::new(&calls);
    system.outcome = Outcome::Fails;
    let error = prepared(&calls, system).capture().unwrap_err();
    assert_eq!(error.stage(), CaptureStage::Capture);
    assert_eq!(
        error.to_string(),
        "failed to request the capture: the time state is unavailable"
    );
    assert!(std::error::Error::source(&error).is_some());
    assert_eq!(
        calls.take(),
        [
            Call::GenerationId,
            Call::Capture(platform_request()),
            Call::CancelCapture,
        ]
    );
}

#[test]
fn a_failed_capture_whose_detectors_cannot_be_restored_is_a_release_error() {
    let release = "failed to restore the stall detectors: read /run/nvx/time-suppression: No such \
                   file or directory";
    let calls = Calls::default();
    let failing = |calls: &Calls| {
        let mut system = FakeSystem::new(calls);
        system.restoration_fails = true;
        system
    };
    let mut system = failing(&calls);
    system.generation_fails = true;
    let error = prepared(&calls, system).capture().unwrap_err();
    assert_eq!(error.stage(), CaptureStage::Release);
    assert_eq!(error.to_string(), release);
    assert_eq!(
        calls.take(),
        [Call::GenerationId, Call::Open, Call::CancelCapture]
    );

    let mut system = failing(&calls);
    system.outcome = Outcome::Fails;
    let error = prepared(&calls, system).capture().unwrap_err();
    assert_eq!(error.stage(), CaptureStage::Release);
    assert_eq!(error.to_string(), release);
    let request_calls = [
        Call::GenerationId,
        Call::Capture(platform_request()),
        Call::CancelCapture,
    ];
    assert_eq!(calls.take(), request_calls);

    // A permission for the ports that the thread may still hold takes precedence.
    let mut system = failing(&calls);
    system.outcome = Outcome::PortRelease;
    let error = prepared(&calls, system).capture().unwrap_err();
    assert_eq!(error.stage(), CaptureStage::Capture);
    assert!(
        matches!(error.cause(), CaptureCause::Time(TimeError::PortRelease(_))),
        "{error:?}"
    );
    assert_eq!(calls.take(), request_calls);
}

#[test]
fn errors_name_their_stage_and_their_cause() {
    let error = CaptureError::new(CaptureStage::Suppression, CaptureCause::Pending);
    assert_eq!(
        error.to_string(),
        "failed to suppress the stall detectors: a capture is already pending"
    );
    assert!(std::error::Error::source(&error).is_none());
    assert!(matches!(error.into_cause(), CaptureCause::Pending));
    let stages = [
        (CaptureStage::Fence, "fence workload admission"),
        (CaptureStage::Mounts, "check the mount table"),
        (
            CaptureStage::BootChecks,
            "wait for the time ABI's boot checks",
        ),
        (
            CaptureStage::CaptureChecks,
            "run the time ABI's capture checks",
        ),
        (CaptureStage::Suppression, "suppress the stall detectors"),
        (CaptureStage::GenerationId, "read the VM generation ID"),
        (CaptureStage::Capture, "request the capture"),
        (CaptureStage::Release, "restore the stall detectors"),
    ];
    for (stage, text) in stages {
        assert_eq!(stage.to_string(), text);
    }
    let origins = [
        (Origin::Distro, "the distro device"),
        (Origin::Scratch, "the scratch device"),
        (Origin::Overlay, "an overlay"),
        (Origin::Filesystem, "the filesystem tag"),
    ];
    for (origin, text) in origins {
        assert_eq!(origin.to_string(), text);
    }
}

/// Writes `contents` to `path`, after creating its parent directories.
fn write(path: &Path, contents: impl AsRef<[u8]>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

#[test]
fn the_live_system_finds_windows_and_block_devices_in_procfs_and_sysfs() {
    let directory = tempfile::tempdir().unwrap();
    let base = directory.path().canonicalize().unwrap();
    let paths = Paths {
        proc: base.join("proc"),
        sys: base.join("sys"),
        dev: base.join("dev"),
    };
    write(
        &paths.proc.join("iomem"),
        format!(
            "00000000-00000fff : Reserved\n  {DISTRO:08x}-{:08x} : virtio-mmio.0\n\
             {SCRATCH:08x}-{:08x} : virtio-mmio.1\n",
            DISTRO + 0xfff,
            SCRATCH + 0xfff,
        ),
    );
    // The distro device has a partition. The kernel lists the scratch's window, but has not added
    // its device.
    let window = paths.sys.join("devices/virtio-mmio-cmdline/virtio-mmio.0");
    let disk = window.join("virtio0/block/vda");
    fs::create_dir_all(disk.join("vda2")).unwrap();
    let blocks = paths.sys.join("dev/block");
    fs::create_dir_all(&blocks).unwrap();
    let target = "../../devices/virtio-mmio-cmdline/virtio-mmio.0/virtio0/block/vda";
    symlink(target, blocks.join("254:0")).unwrap();
    symlink(format!("{target}/vda2"), blocks.join("254:2")).unwrap();
    let mut system = LiveSystem {
        paths: paths.clone(),
        abi: TimeAbi::report_only(),
    };
    assert_eq!(system.windows(DISTRO).unwrap(), [window]);
    assert_eq!(system.windows(SCRATCH).unwrap(), Vec::<PathBuf>::new());
    assert_eq!(system.windows(0x1000).unwrap(), Vec::<PathBuf>::new());
    assert_eq!(system.block_device(number(254, 0)).unwrap(), disk);
    assert_eq!(
        system.block_device(number(254, 2)).unwrap(),
        disk.join("vda2")
    );
    let error = system.block_device(number(8, 0)).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert_eq!(
        error.to_string(),
        format!("{}: {}", blocks.join("8:0").display(), Errno::ENOENT.desc())
    );

    // The check finds a layer in the partition, from the live system's answers.
    let table =
        format!("{TEMPLATE_TABLE}30 23 254:2 / /run/nvx/layers/p2 ro - ext4 /dev/vda2 ro\n");
    let mounted = mounts::first_blocking(&mut system, &devices(), table.as_bytes())
        .unwrap()
        .unwrap();
    assert_eq!(mounted.origin(), Origin::Distro);
    assert_eq!(mounted.mount_point(), Path::new("/run/nvx/layers/p2"));

    // The live system reads one byte more than the limit of a longer table, which the check
    // refuses.
    write(
        &paths.proc.join("self/mountinfo"),
        vec![b'\n'; MOUNT_TABLE_LIMIT + 10],
    );
    assert_eq!(system.mount_table().unwrap().len(), MOUNT_TABLE_LIMIT + 1);
    write(&paths.proc.join("self/mountinfo"), TEMPLATE_TABLE);
    assert_eq!(system.mount_table().unwrap(), TEMPLATE_TABLE.as_bytes());

    // A failed read names its file.
    fs::remove_file(paths.proc.join("iomem")).unwrap();
    let error = system.windows(DISTRO).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert_eq!(
        error.to_string(),
        format!(
            "{}: {}",
            paths.proc.join("iomem").display(),
            Errno::ENOENT.desc()
        )
    );
    fs::remove_file(paths.proc.join("self/mountinfo")).unwrap();
    assert_eq!(
        system.mount_table().unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
}

#[test]
fn the_capture_and_the_repair_take_the_time_abi_in_strict_mode() {
    let abi = live_abi();
    // Strict, unless the kernel command line of the test's host makes the time ABI report-only.
    assert_ne!(abi.is_strict(), abi.is_report_only());
    assert_eq!(abi, TimeAbi::new().strict());
}
