//! Tests of a capture's mount checks, with a fake system whose mount table and devices each test
//! chooses, which records its calls in a log, of the names of a capture's stages, causes, and
//! failures, and of the live system's lookups in a temporary directory.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use super::*;
use crate::guest::snapshot::{self as mounts, CaptureFailure, Devices, Disposition};
use crate::guest::sys::Errno;

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

/// A call of a test's system.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    /// The mount table was read.
    MountTable,
    /// The windows at an MMIO base were looked up.
    Windows(u64),
    /// A block device was looked up by its number.
    BlockDevice(DeviceNumber),
}

/// The log of the calls of a test's system.
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

/// A system that records its calls, with a mount table and devices that a test chooses: by
/// default, the template's table, the distro's and the scratch's windows, a distro device with
/// partitions, a scratch device, and two other devices. The mount checks ask nothing of its time
/// ABI.
struct FakeSystem {
    /// The log.
    calls: Calls,
    /// The mount table, or the error number of its read.
    table: Result<Vec<u8>, Errno>,
    /// The sysfs directories of the windows at each MMIO base.
    windows: HashMap<u64, Vec<PathBuf>>,
    /// The sysfs directory of each block device.
    devices: HashMap<DeviceNumber, PathBuf>,
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

    fn pre_capture_until(&mut self, _deadline: Instant) -> Result<(), (PreCaptureError, bool)> {
        unreachable!("the mount checks prepare no capture")
    }

    fn cancel_capture(&mut self) -> Result<(), Vec<u8>> {
        unreachable!("the mount checks restore no stall detectors")
    }

    fn generation_id(&mut self) -> Result<GenerationId, TimeError> {
        unreachable!("the mount checks read no VM generation ID")
    }

    fn capture(&mut self, _request: CaptureRequest) -> Result<CaptureOutcome, (TimeError, bool)> {
        unreachable!("the mount checks request no capture")
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
        distro: DISTRO,
        scratch: SCRATCH,
        filesystem_tag: TAG.to_owned(),
    }
}

/// Checks the mount table of `system` for the template's devices, as a capture does before it
/// prepares anything.
fn check(system: &mut FakeSystem) -> Result<(), CaptureCause> {
    mounts::check_mounts(system, &devices(), None)
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
        let mut system = FakeSystem::new(&calls).with_lines(&format!("{line}\n"));
        let cause = check(&mut system).unwrap_err();
        let CaptureCause::Mounted(mounted) = &cause else {
            panic!("{cause:?}");
        };
        assert_eq!(mounted.origin(), origin, "{line}");
        assert_eq!(mounted.mount_point(), Path::new(mount_point));
        assert_eq!(
            cause.to_string(),
            format!("{mount_point} is mounted from {origin}")
        );
    }
}

#[test]
fn the_first_blocking_mount_of_the_table_is_reported() {
    let calls = Calls::default();
    let mut system = FakeSystem::new(&calls).with_lines(
        "30 23 0:30 / /run/nvx/rootfs rw - overlay overlay rw\n\
         31 23 254:0 / /run/nvx/layers/distro ro - erofs /dev/vda ro\n",
    );
    let cause = check(&mut system).unwrap_err();
    assert_eq!(
        cause.to_string(),
        "/run/nvx/rootfs is mounted from an overlay"
    );
    assert_eq!(
        calls.take(),
        [
            Call::MountTable,
            Call::Windows(DISTRO),
            Call::Windows(SCRATCH),
        ]
    );
}

#[test]
fn mounts_from_other_devices_and_tags_block_no_capture() {
    let calls = Calls::default();
    // Another tag's export, a tmpfs whose source names the tag or an overlay, a file system on
    // another block device, and one whose source names the distro's node but whose device is
    // another's, below a window whose name only starts with the scratch's.
    let mut system = FakeSystem::new(&calls).with_lines(
        "30 23 0:31 / /mnt/other ro - virtiofs other ro\n\
         31 23 0:32 / /mnt/tag rw - tmpfs microvm rw\n\
         32 23 0:33 / /mnt/overlay rw - tmpfs overlay rw\n\
         33 23 254:32 / /mnt/vdc rw - ext4 /dev/vdc rw\n\
         34 23 254:48 / /mnt/vdd rw - ext4 /dev/vda rw\n",
    );
    check(&mut system).unwrap();
    assert_eq!(
        calls.take(),
        [
            Call::MountTable,
            Call::Windows(DISTRO),
            Call::Windows(SCRATCH),
            Call::BlockDevice(number(254, 32)),
            Call::BlockDevice(number(254, 48)),
        ]
    );
}

#[test]
fn a_device_without_a_window_has_nothing_mounted_from_it() {
    let calls = Calls::default();
    let mut system =
        FakeSystem::new(&calls).with_lines("30 23 254:32 / /mnt/vdc rw - ext4 /dev/vdc rw\n");
    // The kernel lists no window at the scratch's base, as in a VM without a scratch device.
    system.windows.remove(&SCRATCH);
    system.devices.remove(&number(254, 16));
    check(&mut system).unwrap();
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
        let cause = check(&mut system).unwrap_err();
        let CaptureCause::Io(error) = &cause else {
            panic!("{cause:?}");
        };
        assert_eq!(error.kind(), kind, "{cause}");
        assert_eq!(cause.to_string(), text);
    }
    // A table of the limit's length is read whole.
    let calls = Calls::default();
    let mut system = FakeSystem::new(&calls);
    system.table = Ok(vec![b'\n'; MOUNT_TABLE_LIMIT]);
    check(&mut system).unwrap();
}

#[test]
fn errors_name_their_stage_and_their_cause() {
    let failure = CaptureFailure {
        stage: CaptureStage::Suppression,
        cause: CaptureCause::Pending,
        disposition: Disposition::Resumed,
    };
    assert_eq!(
        failure.to_string(),
        "failed to suppress the stall detectors: a capture is already pending"
    );
    assert!(std::error::Error::source(&failure).is_none());
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
        deadline: None,
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
    let canonical = mounts::Devices {
        distro: DISTRO,
        scratch: SCRATCH,
        filesystem_tag: TAG.to_owned(),
    };
    let mounted = mounts::first_blocking(&mut system, &canonical, table.as_bytes(), None)
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
