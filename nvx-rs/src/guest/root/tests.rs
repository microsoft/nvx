//! Tests of the root operations with the fake kernel, and with the kernel's interfaces, a cgroup
//! hierarchy, and the runtime directory in a temporary directory, and of the kernel's own check
//! for a mount point.

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::kernel::{Call, FakeKernel, Operation, Paths, super_options};
use super::*;
use crate::gpt::ExpectedPartition;
use crate::guest::cgroup::Layout;
use crate::guest::mappings::{Access, Mapping, SourceRoot};
use crate::guest::sys::MountAttributes;

/// The base address of the distro device's MMIO window.
const DISTRO: u64 = 0xd000_0000;
/// The base address of the scratch device's MMIO window.
const SCRATCH: u64 = 0x1_d000_1000;
/// The UUID of the test's EROFS image.
const UUID: Uuid = Uuid::from_bytes([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
/// The GUID of the tests' GPT disks.
const DISK_GUID: Uuid = Uuid::from_bytes([0xD1; 16]);
/// The partition type GUID of the tests' GPT partitions: Linux file system data,
/// `0fc63daf-8483-4772-8e79-3d69d8477de4`.
const LINUX_DATA: Uuid = Uuid::from_bytes([
    0x0f, 0xc6, 0x3d, 0xaf, 0x84, 0x83, 0x47, 0x72, 0x8e, 0x79, 0x3d, 0x69, 0xd8, 0x47, 0x7d, 0xe4,
]);
/// The size of the tests' GPT disks' logical blocks, in bytes.
const BLOCK: usize = 512;
/// The size of each layer of the tests' GPT disks, in blocks: the first 2 KiB of an ext4 file
/// system, which hold its superblock.
const LAYER_BLOCKS: usize = 4;
/// The tests' bound for the first partition, in bytes.
const FIRST_PARTITION_BOUND: usize = 64 * 1024;

/// A guest in a temporary directory, and the fake kernel that the operations get clones of.
struct Guest {
    /// The temporary directory, which the guest holds only so that dropping it removes the
    /// directory.
    _directory: tempfile::TempDir,
    /// The temporary directory's canonical path, which no symbolic link leads through, as none
    /// may lead through a runtime directory.
    base: PathBuf,
    /// The kernel's interfaces, below the temporary directory.
    paths: Paths,
    /// The fake kernel.
    kernel: FakeKernel,
}

impl Guest {
    /// Returns a guest with the distro device at window 0 and the scratch at window 1, both
    /// attached, and nothing else.
    fn new() -> Self {
        let guest = Self::bare();
        attach(&guest.base, 0, "vda", 4096);
        attach(&guest.base, 1, "vdb", 8192);
        guest
    }

    /// Returns a guest whose `/proc/iomem` declares the distro device's window 0 and the
    /// scratch's window 1, without their devices.
    fn bare() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().canonicalize().unwrap();
        let paths = Paths {
            proc: base.join("proc"),
            sys: base.join("sys"),
            dev: base.join("dev"),
        };
        fs::create_dir_all(&paths.dev).unwrap();
        write(
            &paths.proc.join("iomem"),
            format!(
                "00000000-00000fff : Reserved\n  {DISTRO:08x}-{:08x} : virtio-mmio.0\n\
                 {SCRATCH:016x}-{:016x} : virtio-mmio.1\n",
                DISTRO + 0x1ff,
                SCRATCH + 0x1ff,
            ),
        );
        // The defaults of the overlay's options, as the guest's kernel sets them: all off.
        for parameter in [
            "metacopy",
            "redirect_dir",
            "redirect_always_follow",
            "xino_auto",
        ] {
            write(&paths.sys.join(OVERLAY_PARAMETERS).join(parameter), "N\n");
        }
        Self {
            _directory: directory,
            base,
            paths,
            kernel: FakeKernel::default(),
        }
    }

    /// Returns `relative`'s path below the temporary directory.
    fn path(&self, relative: &str) -> PathBuf {
        self.base.join(relative)
    }

    /// Returns `relative`'s path below the temporary directory, as the fake kernel records it.
    fn text(&self, relative: &str) -> String {
        self.path(relative).display().to_string()
    }

    /// Returns an assembly of the guest's devices, in its runtime directory, `run/nvx`: an EROFS
    /// image over an ext4 scratch that may have a journal, with the default overlay mode.
    fn assembly(&self) -> Assembly {
        Assembly {
            runtime: self.path("run/nvx"),
            distro: Device {
                mmio: DISTRO,
                sectors: None,
            },
            image: Image::Erofs { uuid: None },
            upper: scratch_upper(Journal::Allowed),
            overlay: OverlayMode::TrustedLowers,
            timeout: Duration::from_millis(100),
        }
    }

    /// Returns an assembly of a GPT disk on the guest's distro device, with the tests' bound for
    /// the first partition and no other expectation, over `upper`, with the overlay mode
    /// `overlay`.
    fn gpt_assembly(&self, upper: Upper, overlay: OverlayMode) -> Assembly {
        Assembly {
            image: Image::Gpt {
                expected: Expected::new(),
                first_partition_bound: FIRST_PARTITION_BOUND,
            },
            upper,
            overlay,
            ..self.assembly()
        }
    }

    /// Writes `disk` to the distro device, and creates the device nodes of its layers'
    /// partitions, with their layers' bytes, as devtmpfs does once the kernel has read the disk's
    /// table.
    fn insert(&self, disk: &GptDisk) {
        fs::write(self.path("dev/vda"), disk.image()).unwrap();
        for number in 2..=disk.partitions() {
            write(&self.path(&format!("dev/vda{number}")), disk.layer(number));
        }
    }

    /// Assembles `assembly` with a clone of the guest's kernel, as [`assemble`] does.
    fn assemble(&self, assembly: &Assembly) -> Result<Root, AssemblyError> {
        assemble_with(
            Box::new(self.kernel.clone()),
            &self.paths,
            assembly,
            None,
            Contents::Kept,
        )
    }

    /// Assembles `assembly` with a clone of the guest's kernel, waiting for each device until
    /// `deadline` at the latest.
    fn assemble_until(
        &self,
        assembly: &Assembly,
        deadline: Instant,
    ) -> Result<Root, AssemblyError> {
        let kernel = Box::new(self.kernel.clone());
        assemble_with(
            kernel,
            &self.paths,
            assembly,
            Some(deadline),
            Contents::Kept,
        )
    }

    /// Assembles `assembly` with a clone of the guest's kernel and a fresh upper, as
    /// [`assemble_fresh`] does.
    fn assemble_fresh(&self, assembly: &Assembly) -> Result<Root, AssemblyError> {
        assemble_with(
            Box::new(self.kernel.clone()),
            &self.paths,
            assembly,
            None,
            Contents::Fresh,
        )
    }

    /// Sets the overlay module's parameter `name`, the default of one of the overlay's options:
    /// on, or off.
    fn set_overlay_default(&self, name: &str, on: bool) {
        let parameter = self.paths.sys.join(OVERLAY_PARAMETERS).join(name);
        write(&parameter, if on { "Y\n" } else { "N\n" });
    }

    /// Makes the kernel show, as the options of the overlay of an assembly of the guest's EROFS
    /// image over `upper`, its `access` and its directories, then `options`, if any.
    fn show_overlay(&self, access: &str, upper: UpperKind, options: &str) {
        let runtime = self.text("run/nvx");
        let point = upper_point(upper);
        let mut shown = format!(
            "{access},lowerdir={runtime}/layers/distro,upperdir={runtime}/{point}/upper,\
             workdir={runtime}/{point}/work"
        );
        if !options.is_empty() {
            shown = format!("{shown},{options}");
        }
        self.kernel.show(self.path("run/nvx/rootfs"), &shown);
    }
}

/// Returns the mount point of an upper of `kind`, below the runtime directory.
fn upper_point(kind: UpperKind) -> &'static str {
    match kind {
        UpperKind::Scratch => "scratch",
        UpperKind::Tmpfs => "tmpfs",
    }
}

/// Returns the upper on the guest's scratch device, with `journal`.
fn scratch_upper(journal: Journal) -> Upper {
    Upper::Scratch {
        device: Device {
            mmio: SCRATCH,
            sectors: None,
        },
        journal,
    }
}

/// Returns the scratch device of `assembly`, whose upper must be an ext4 scratch.
fn scratch(assembly: &mut Assembly) -> &mut Device {
    match &mut assembly.upper {
        Upper::Scratch { device, .. } => device,
        upper => panic!("{upper:?}"),
    }
}

/// Writes `contents` to `path`, after creating its parent directories.
fn write(path: &Path, contents: impl AsRef<[u8]>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// Attaches the block device `name`, of `sectors` sectors, below the MMIO window `index` of the
/// guest in `root`: its sysfs directory, its size and access mode, and its node.
fn attach(root: &Path, index: usize, name: &str, sectors: u64) {
    probe(root, index, name, sectors);
    write(&root.join("dev").join(name), "");
}

/// Creates the sysfs directory, size, and access mode of the block device `name` below the MMIO
/// window `index` of the guest in `root`, without its node. The device is read-only at window 0,
/// where the tests attach the distro device, and writable elsewhere, as at the scratch's window 1.
fn probe(root: &Path, index: usize, name: &str, sectors: u64) {
    let window = format!("sys/devices/virtio-mmio-cmdline/virtio-mmio.{index}");
    fs::create_dir_all(
        root.join(window)
            .join(format!("virtio{index}/block/{name}")),
    )
    .unwrap();
    set_read_only(root, name, index == 0);
    write(
        &root.join("sys/class/block").join(name).join("size"),
        format!("{sectors}\n"),
    );
}

/// Sets the access mode of the block device `name` of the guest in `root`: read-only, or
/// writable.
fn set_read_only(root: &Path, name: &str, read_only: bool) {
    write(
        &root.join("sys/class/block").join(name).join("ro"),
        if read_only { "1\n" } else { "0\n" },
    );
}

/// A GPT disk of 512-byte blocks that a test writes, with the tests' disk GUID: its first
/// partition, which holds the test's bytes in as many blocks as they take, and at least one, then
/// the layers, of [`LAYER_BLOCKS`] blocks each, the start of an ext4 file system without a
/// journal, whose first byte is its partition's number. Partition N's unique GUID is
/// [`GptDisk::guid`]`(N)`.
struct GptDisk {
    /// The first partition's bytes.
    first_partition: Vec<u8>,
    /// The layers' bytes, partition 2's first.
    layers: Vec<Vec<u8>>,
}

impl GptDisk {
    /// Returns a disk whose first partition holds `first_partition`, with `layers` layers.
    fn new(first_partition: &[u8], layers: u8) -> Self {
        let layers = (2..=layers.saturating_add(1))
            .map(|number| {
                let mut layer = ext4_image(false);
                layer[0] = number;
                layer
            })
            .collect();
        Self {
            first_partition: first_partition.to_vec(),
            layers,
        }
    }

    /// Returns the number of partitions, the first included.
    fn partitions(&self) -> u8 {
        u8::try_from(self.layers.len() + 1).unwrap()
    }

    /// Returns the bytes of the layer in partition `number`.
    fn layer(&self, number: u8) -> &[u8] {
        &self.layers[usize::from(number) - 2]
    }

    /// Returns the bytes of the layer in partition `number`, for a test to change.
    fn layer_mut(&mut self, number: u8) -> &mut [u8] {
        &mut self.layers[usize::from(number) - 2]
    }

    /// Returns the unique GUID of partition `number`.
    fn guid(number: u8) -> Uuid {
        let mut bytes = [0x11; 16];
        bytes[15] = number;
        Uuid::from_bytes(bytes)
    }

    /// Returns the number of the first partition's blocks.
    fn first_partition_blocks(&self) -> usize {
        self.first_partition.len().div_ceil(BLOCK).max(1)
    }

    /// Returns the size of partition `number`, in bytes.
    fn size(&self, number: u8) -> u64 {
        let blocks = if number == 1 {
            self.first_partition_blocks()
        } else {
            LAYER_BLOCKS
        };
        (blocks * BLOCK) as u64
    }

    /// Writes the disk: its protective MBR, its primary header and partition entry array, and its
    /// partitions, without the backup table, which the reader never reads.
    fn image(&self) -> Vec<u8> {
        // At least UEFI's 128 entries, four to a block.
        let entries = usize::from(self.partitions()).max(128).next_multiple_of(4);
        let array_blocks = entries * 128 / BLOCK;
        let first_usable = 2 + array_blocks;
        let last_usable =
            first_usable + self.first_partition_blocks() + self.layers.len() * LAYER_BLOCKS - 1;
        // The alternate header, after the room for its array.
        let alternate = last_usable + array_blocks + 1;
        let mut image = vec![0; (alternate + 1) * BLOCK];
        image[446 + 4] = 0xEE;
        image[446 + 8..446 + 12].copy_from_slice(&1u32.to_le_bytes());
        let size = u32::try_from(alternate).unwrap();
        image[446 + 12..446 + 16].copy_from_slice(&size.to_le_bytes());
        image[510..512].copy_from_slice(&[0x55, 0xAA]);
        let mut array = vec![0; entries * 128];
        let mut lba = first_usable;
        for number in 1..=self.partitions() {
            let blocks = self.size(number) as usize / BLOCK;
            let entry = &mut array[(usize::from(number) - 1) * 128..][..128];
            entry[..16].copy_from_slice(&LINUX_DATA.to_bytes_le());
            entry[16..32].copy_from_slice(&Self::guid(number).to_bytes_le());
            entry[32..40].copy_from_slice(&(lba as u64).to_le_bytes());
            entry[40..48].copy_from_slice(&((lba + blocks - 1) as u64).to_le_bytes());
            let bytes = if number == 1 {
                &self.first_partition
            } else {
                self.layer(number)
            };
            image[lba * BLOCK..][..bytes.len()].copy_from_slice(bytes);
            lba += blocks;
        }
        image[2 * BLOCK..][..array.len()].copy_from_slice(&array);
        let header = &mut image[BLOCK..2 * BLOCK];
        header[..8].copy_from_slice(b"EFI PART");
        header[8..12].copy_from_slice(&0x0001_0000_u32.to_le_bytes());
        header[12..16].copy_from_slice(&92_u32.to_le_bytes());
        header[24..32].copy_from_slice(&1_u64.to_le_bytes());
        header[32..40].copy_from_slice(&(alternate as u64).to_le_bytes());
        header[40..48].copy_from_slice(&(first_usable as u64).to_le_bytes());
        header[48..56].copy_from_slice(&(last_usable as u64).to_le_bytes());
        header[56..72].copy_from_slice(&DISK_GUID.to_bytes_le());
        header[72..80].copy_from_slice(&2_u64.to_le_bytes());
        header[80..84].copy_from_slice(&u32::try_from(entries).unwrap().to_le_bytes());
        header[84..88].copy_from_slice(&128_u32.to_le_bytes());
        header[88..92].copy_from_slice(&crc32(&array).to_le_bytes());
        let crc = crc32(&header[..92]);
        header[16..20].copy_from_slice(&crc.to_le_bytes());
        image
    }
}

/// Returns the CRC-32 of `bytes` that GPT records: IEEE 802.3's, reflected.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Returns the first 2 KiB of an ext4 file system, whose superblock has the `has_journal`
/// feature if `journal`.
fn ext4_image(journal: bool) -> Vec<u8> {
    let mut image = vec![0; 2048];
    image[1024 + 0x38..1024 + 0x3A].copy_from_slice(&0xEF53_u16.to_le_bytes());
    if journal {
        image[1024 + 0x5C] = 0x04;
    }
    image
}

/// Returns an EROFS image's first 4 KiB, whose superblock holds `uuid`.
fn erofs_image(uuid: Uuid) -> Vec<u8> {
    let mut image = vec![0; 4096];
    image[1024..1028].copy_from_slice(&0xE0F5_E1E2_u32.to_le_bytes());
    image[1024 + 48..1024 + 64].copy_from_slice(uuid.as_bytes());
    image
}

/// Returns the error of a failed system call, as an operation reports it.
fn os_error(errno: Errno) -> io::Error {
    io::Error::from_raw_os_error(errno as i32)
}

/// Returns a recorded mount.
fn mount_call(
    source: Option<&str>,
    target: &str,
    filesystem: Option<&str>,
    flags: MountFlags,
    data: Option<&str>,
) -> Call {
    Call::Mount {
        source: source.map(str::to_owned),
        target: target.to_owned(),
        filesystem: filesystem.map(str::to_owned),
        flags,
        data: data.map(str::to_owned),
    }
}

#[test]
fn assembly_mounts_the_image_and_the_scratch_under_the_overlay() {
    let guest = Guest::new();
    let root = guest.assemble(&guest.assembly()).unwrap();
    let runtime = guest.text("run/nvx");
    let options = format!(
        "lowerdir={runtime}/layers/distro,upperdir={runtime}/scratch/upper,\
         workdir={runtime}/scratch/work,metacopy=on,xino=on"
    );
    // Both devices are checked and flushed before anything is mounted.
    assert_eq!(
        guest.kernel.calls(),
        [
            Call::Flush(guest.text("dev/vda")),
            Call::Flush(guest.text("dev/vdb")),
            mount_call(
                Some(&guest.text("dev/vda")),
                &format!("{runtime}/layers/distro"),
                Some("erofs"),
                MountFlags::MS_RDONLY,
                None,
            ),
            mount_call(
                Some(&guest.text("dev/vdb")),
                &format!("{runtime}/scratch"),
                Some("ext4"),
                MountFlags::MS_NOSUID | MountFlags::MS_NODEV,
                None,
            ),
            mount_call(
                Some("overlay"),
                &format!("{runtime}/rootfs"),
                Some("overlay"),
                MountFlags::MS_NOSUID | MountFlags::MS_NODEV,
                Some(&options),
            ),
        ]
    );
    for directory in ["layers/distro", "scratch/upper", "scratch/work", "rootfs"] {
        assert!(
            guest.path("run/nvx").join(directory).is_dir(),
            "{directory}"
        );
    }
    assert_eq!(root.path(), guest.path("run/nvx/rootfs"));
    assert_eq!(root.runtime(), guest.path("run/nvx"));
    // The root reports what it mounted: the image's one layer, the scratch, and the mode.
    assert_eq!(root.first_partition(), None);
    let [lower] = root.lowers() else {
        panic!("{:?}", root.lowers());
    };
    assert_eq!(lower.mount(), &Mount::Distro);
    assert_eq!(lower.device(), guest.path("dev/vda"));
    assert_eq!(lower.path(), guest.path("run/nvx/layers/distro"));
    assert_eq!(lower.partition(), None);
    assert_eq!(root.upper(), UpperKind::Scratch);
    assert_eq!(root.overlay(), OverlayMode::TrustedLowers);
    root.teardown().unwrap();
}

#[test]
fn devices_are_found_when_they_appear_and_missing_ones_time_out() {
    let guest = Guest::bare();
    attach(&guest.base, 1, "vdb", 8192);
    let root_directory = guest.base.clone();
    // The kernel probes the device first, then creates its node.
    guest.kernel.on_sleep(move |sleeps| match sleeps {
        1 => probe(&root_directory, 0, "vda", 4096),
        2 => write(&root_directory.join("dev/vda"), ""),
        _ => {}
    });
    let mut assembly = guest.assembly();
    assembly.timeout = Duration::from_secs(1);
    guest.assemble(&assembly).unwrap().teardown().unwrap();
    assert_eq!(guest.kernel.sleeps(), 2);

    // A timeout of 100 ms allows two more polls, 50 ms apart.
    let guest = Guest::bare();
    attach(&guest.base, 0, "vda", 4096);
    let error = guest.assemble(&guest.assembly()).unwrap_err();
    assert!(
        matches!(
            error.error(),
            Error::Missing {
                role: Role::Scratch,
                mmio: SCRATCH,
            }
        ),
        "{error:?}"
    );
    assert!(error.cleanup().is_none());
    assert_eq!(guest.kernel.sleeps(), 2);
    assert_eq!(
        error.to_string(),
        "the scratch device at MMIO 0x1d0001000 did not appear"
    );
    // The assembly checks the scratch device before it mounts anything.
    assert!(guest.kernel.mounts().is_empty());
    assert!(guest.kernel.unmounts().is_empty());
}

#[test]
fn an_assembly_waits_for_its_devices_until_its_deadline_at_the_latest() {
    // However long its timeout, an assembly whose deadline passed looks for each device once,
    // without sleeping, and finds a missing one missing, before it mounts anything.
    let guest = Guest::bare();
    attach(&guest.base, 0, "vda", 4096);
    let mut assembly = guest.assembly();
    assembly.timeout = Duration::from_secs(600);
    let error = guest.assemble_until(&assembly, Instant::now()).unwrap_err();
    assert!(
        matches!(
            error.error(),
            Error::Missing {
                role: Role::Scratch,
                mmio: SCRATCH,
            }
        ),
        "{error:?}"
    );
    assert_eq!(guest.kernel.sleeps(), 0);
    assert!(guest.kernel.mounts().is_empty());

    // One whose deadline is a second away polls, 50 ms apart, until the deadline rather than
    // until its timeout's end.
    let guest = Guest::bare();
    attach(&guest.base, 0, "vda", 4096);
    let deadline = Instant::now() + Duration::from_secs(1);
    let error = guest.assemble_until(&assembly, deadline).unwrap_err();
    assert!(
        matches!(
            error.error(),
            Error::Missing {
                role: Role::Scratch,
                ..
            }
        ),
        "{error:?}"
    );
    let sleeps = guest.kernel.sleeps();
    assert!((1..=20).contains(&sleeps), "{sleeps}");

    // The deadline bounds the polls in time too, whatever a poll or a sleep takes: a sleep that
    // overruns it is the last.
    let guest = Guest::bare();
    attach(&guest.base, 0, "vda", 4096);
    guest
        .kernel
        .on_sleep(|_| std::thread::sleep(Duration::from_millis(600)));
    let deadline = Instant::now() + Duration::from_millis(500);
    let error = guest.assemble_until(&assembly, deadline).unwrap_err();
    assert!(
        matches!(
            error.error(),
            Error::Missing {
                role: Role::Scratch,
                ..
            }
        ),
        "{error:?}"
    );
    let sleeps = guest.kernel.sleeps();
    assert!(sleeps <= 1, "{sleeps}");

    // A device that appears before the deadline is found.
    let guest = Guest::bare();
    attach(&guest.base, 0, "vda", 4096);
    let root_directory = guest.base.clone();
    guest.kernel.on_sleep(move |sleeps| {
        if sleeps == 1 {
            attach(&root_directory, 1, "vdb", 8192);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(1);
    let root = guest.assemble_until(&assembly, deadline).unwrap();
    assert_eq!(guest.kernel.sleeps(), 1);
    root.teardown().unwrap();

    // So is one that appears while the assembly sleeps less than an interval, until a deadline
    // that comes before the interval ends, at which it polls once more.
    let guest = Guest::bare();
    attach(&guest.base, 0, "vda", 4096);
    let root_directory = guest.base.clone();
    guest.kernel.on_sleep(move |sleeps| {
        if sleeps == 1 {
            attach(&root_directory, 1, "vdb", 8192);
        }
    });
    let wait = Duration::from_millis(40);
    let started = Instant::now();
    match guest.assemble_until(&assembly, started + wait) {
        Ok(root) => {
            assert_eq!(guest.kernel.sleeps(), 1);
            root.teardown().unwrap();
        }
        // Unless the test ran so late that the deadline passed before the assembly looked for
        // the scratch.
        Err(error) => assert!(started.elapsed() >= wait, "{error:?}"),
    }
}

#[test]
fn a_device_of_another_size_is_refused_before_it_is_flushed() {
    let guest = Guest::new();
    let mut assembly = guest.assembly();
    assembly.distro.sectors = Some(2048);
    let error = guest.assemble(&assembly).unwrap_err().into_parts().0;
    assert!(
        matches!(
            &error,
            Error::Size {
                role: Role::Distro,
                device,
                expected: 2048,
                actual: 4096,
            } if *device == guest.path("dev/vda")
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "the distro device {} has 4096 sectors, expected 2048",
            guest.text("dev/vda")
        )
    );
    assert!(guest.kernel.calls().is_empty());

    assembly.distro.sectors = Some(4096);
    scratch(&mut assembly).sectors = Some(8192);
    guest.assemble(&assembly).unwrap().teardown().unwrap();
}

#[test]
fn the_expected_uuid_is_checked_in_the_superblock_before_the_mount() {
    let guest = Guest::new();
    fs::write(guest.path("dev/vda"), erofs_image(UUID)).unwrap();
    let mut assembly = guest.assembly();
    assembly.image = Image::Erofs { uuid: Some(UUID) };
    guest.assemble(&assembly).unwrap().teardown().unwrap();

    let guest = Guest::new();
    fs::write(guest.path("dev/vda"), erofs_image(UUID)).unwrap();
    let mut bytes = *UUID.as_bytes();
    bytes[15] = 0xff;
    let expected = Uuid::from_bytes(bytes);
    assembly = guest.assembly();
    assembly.image = Image::Erofs {
        uuid: Some(expected),
    };
    let error = guest.assemble(&assembly).unwrap_err();
    assert!(
        matches!(error.error(), Error::Uuid { expected: e, actual: UUID } if *e == expected),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "the distro layer's UUID is 01020304-0506-0708-090a-0b0c0d0e0f10, \
         expected 01020304-0506-0708-090a-0b0c0d0e0fff"
    );
    assert!(guest.kernel.mounts().is_empty());
    assert!(guest.kernel.unmounts().is_empty());

    // A device without the EROFS magic number has no UUID to check.
    let guest = Guest::new();
    fs::write(guest.path("dev/vda"), vec![0; 4096]).unwrap();
    let error = guest.assemble(&assembly).unwrap_err();
    assert!(
        matches!(error.error(), Error::NotErofs { device } if *device == guest.path("dev/vda")),
        "{error:?}"
    );
    assert!(guest.kernel.mounts().is_empty());
}

#[test]
fn a_failed_assembly_unmounts_what_it_mounted_and_reports_what_stays() {
    let guest = Guest::new();
    guest.kernel.fail(
        Operation::Mount,
        guest.path("run/nvx/rootfs"),
        Errno::EINVAL,
    );
    let error = guest.assemble(&guest.assembly()).unwrap_err();
    assert!(
        matches!(
            error.error(),
            Error::Mount { mount: Mount::Overlay, error } if error.raw_os_error() == Some(Errno::EINVAL as i32)
        ),
        "{error:?}"
    );
    assert!(error.cleanup().is_none());
    assert_eq!(
        guest.kernel.unmounts(),
        [
            guest.text("run/nvx/layers/distro"),
            guest.text("run/nvx/scratch")
        ]
    );

    // A failed unmount of the cleanup is reported too, and the next one still attempted.
    let guest = Guest::new();
    guest.kernel.fail(
        Operation::Mount,
        guest.path("run/nvx/rootfs"),
        Errno::EINVAL,
    );
    guest.kernel.fail(
        Operation::Unmount,
        guest.path("run/nvx/layers/distro"),
        Errno::EBUSY,
    );
    let error = guest.assemble(&guest.assembly()).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "failed to mount the overlay: {}; then failed to unmount the distro layer: {}",
            os_error(Errno::EINVAL),
            os_error(Errno::EBUSY)
        )
    );
    let (_, cleanup) = error.into_parts();
    let failures = cleanup.unwrap();
    let [failure] = failures.failures() else {
        panic!("{failures:?}");
    };
    assert_eq!(failure.mount(), &Mount::Distro);
    assert_eq!(failure.path(), guest.path("run/nvx/layers/distro"));
    assert_eq!(failure.error().raw_os_error(), Some(Errno::EBUSY as i32));
    assert_eq!(
        guest.kernel.unmounts(),
        [
            guest.text("run/nvx/layers/distro"),
            guest.text("run/nvx/scratch")
        ]
    );
}

#[test]
fn a_runtime_directory_that_the_overlay_options_reserve_is_refused() {
    let guest = Guest::new();
    for runtime in ["run/nvx", "/run/a,b", "/run/a:b", "/run/a\\b", "/run/a=b"] {
        let mut assembly = guest.assembly();
        assembly.runtime = runtime.into();
        let error = guest.assemble(&assembly).unwrap_err();
        assert!(
            matches!(error.error(), Error::Runtime { path } if *path == Path::new(runtime)),
            "{runtime}: {error:?}"
        );
    }
    assert!(guest.kernel.calls().is_empty());
}

#[test]
fn a_runtime_directory_whose_path_is_not_canonical_is_refused() {
    // Once the scratch hid `probe`, the last path would lead nowhere, and the image and the
    // scratch could not be unmounted.
    let guest = Guest::new();
    fs::create_dir_all(guest.path("run/nvx/scratch/probe")).unwrap();
    let parent = guest.text("run/nvx/scratch/probe/../..");
    let forms = [
        "/",
        "/run/nvx/",
        "/run//nvx",
        "/run/./nvx",
        "/run/nvx/.",
        "/run/nvx/..",
    ];
    for runtime in forms.into_iter().chain([parent.as_str()]) {
        let mut assembly = guest.assembly();
        assembly.runtime = runtime.into();
        let error = guest.assemble(&assembly).unwrap_err();
        assert!(
            matches!(error.error(), Error::Runtime { path } if path.as_os_str() == runtime),
            "{runtime}: {error:?}"
        );
        assert!(error.cleanup().is_none());
    }
    assert!(guest.kernel.calls().is_empty());
    assert!(!guest.path("run/nvx/layers").exists());
    let mut assembly = guest.assembly();
    assembly.runtime = parent.clone().into();
    assert_eq!(
        guest.assemble(&assembly).unwrap_err().to_string(),
        format!(
            "the runtime directory is not a canonical absolute path without `,`, `:`, `\\`, `=`, \
             or a symbolic link: {parent}"
        )
    );
}

#[test]
fn a_root_mounts_nothing_through_a_symbolic_link() {
    // A runtime directory that a link leads through, whether the link names it, one of its
    // parents, or a path through the scratch's mount point.
    let guest = Guest::new();
    fs::create_dir_all(guest.path("run/nvx/scratch/probe")).unwrap();
    std::os::unix::fs::symlink("nvx", guest.path("run/link")).unwrap();
    std::os::unix::fs::symlink("run", guest.path("alias")).unwrap();
    std::os::unix::fs::symlink("nvx/scratch/probe/../..", guest.path("run/up")).unwrap();
    for runtime in ["run/link", "alias/nvx", "run/up"].map(|relative| guest.path(relative)) {
        let mut assembly = guest.assembly();
        assembly.runtime = runtime.clone();
        let error = guest.assemble(&assembly).unwrap_err();
        assert!(
            matches!(error.error(), Error::Runtime { path } if *path == runtime),
            "{}: {error:?}",
            runtime.display()
        );
    }
    assert!(guest.kernel.calls().is_empty());
    assert!(!guest.path("run/nvx/layers").exists());

    // A link at one of the assembly's mount points, or at the directory above the image's.
    for name in ["layers", "layers/distro", "scratch", "rootfs"] {
        let guest = Guest::new();
        let link = guest.path("run/nvx").join(name);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        fs::create_dir(guest.path("elsewhere")).unwrap();
        std::os::unix::fs::symlink(guest.path("elsewhere"), &link).unwrap();
        let error = guest.assemble(&guest.assembly()).unwrap_err();
        assert!(
            matches!(error.error(), Error::MountPoint { path, error: cause }
                if *path == link && cause.raw_os_error() == Some(Errno::ENOTDIR as i32)),
            "{name}: {error:?}"
        );
        assert!(guest.kernel.calls().is_empty(), "{name}");
    }

    // A link at an aggregate's mount point, which the aggregate refuses.
    let guest = Guest::new();
    let shares = guest.path("run/nvx/shares");
    fs::create_dir_all(guest.path("run/nvx")).unwrap();
    fs::create_dir(guest.path("elsewhere")).unwrap();
    std::os::unix::fs::symlink(guest.path("elsewhere"), &shares).unwrap();
    let mut root = guest.assemble(&guest.assembly()).unwrap();
    let mounts = guest.kernel.mounts();
    let rw = Access::ReadWrite;
    let error = root
        .mount_aggregate("all", rw, &[child("a", "/a", rw, ShareKind::Directory)])
        .unwrap_err();
    assert!(
        matches!(&error, Error::MountPoint { path, error: cause }
            if *path == shares && cause.raw_os_error() == Some(Errno::ENOTDIR as i32)),
        "{error:?}"
    );
    assert_eq!(guest.kernel.mounts(), mounts);
    root.teardown().unwrap();
}

#[test]
fn new_upper_and_work_directories_have_mode_0755_whatever_the_umask() {
    // The umask is the process's, so the test runs alone in a process of its own.
    if crate::guest::managed_agent::isolate_process_test(
        "NVX_ROOT_UMASK_TEST_CHILD",
        "guest::root::tests::new_upper_and_work_directories_have_mode_0755_whatever_the_umask",
    ) {
        return;
    }
    nix::sys::stat::umask(sys::Mode::from_bits_truncate(0o077));
    let mode = |path: &Path| fs::metadata(path).unwrap().mode() & 0o7777;
    // The overlay's root directory takes the upper's mode, which a workload that is not root
    // must be able to enter.
    let guest = Guest::new();
    guest
        .assemble(&guest.assembly())
        .unwrap()
        .teardown()
        .unwrap();
    assert_eq!(mode(&guest.path("run/nvx/scratch/upper")), 0o755);
    assert_eq!(mode(&guest.path("run/nvx/scratch/work")), 0o755);

    // A directory that the scratch holds already, from an earlier boot, keeps its mode.
    let guest = Guest::new();
    let upper = guest.path("run/nvx/scratch/upper");
    fs::create_dir_all(&upper).unwrap();
    fs::set_permissions(&upper, fs::Permissions::from_mode(0o700)).unwrap();
    guest
        .assemble(&guest.assembly())
        .unwrap()
        .teardown()
        .unwrap();
    assert_eq!(mode(&upper), 0o700);
    assert_eq!(mode(&guest.path("run/nvx/scratch/work")), 0o755);

    // So are a tmpfs upper's, which are always new.
    let guest = Guest::new();
    let assembly = Assembly {
        upper: Upper::Tmpfs,
        ..guest.assembly()
    };
    guest.assemble(&assembly).unwrap().teardown().unwrap();
    assert_eq!(mode(&guest.path("run/nvx/tmpfs/upper")), 0o755);
    assert_eq!(mode(&guest.path("run/nvx/tmpfs/work")), 0o755);
}

#[test]
fn a_fresh_upper_s_directories_must_be_missing_or_empty() {
    let mode = |path: &Path| fs::metadata(path).unwrap().mode() & 0o7777;
    // A scratch or a tmpfs without the directories gets them, as any root's upper does, with no
    // other call than a cold root's.
    for upper in [scratch_upper(Journal::Allowed), Upper::Tmpfs] {
        let guest = Guest::new();
        let assembly = Assembly {
            upper,
            ..guest.assembly()
        };
        guest.assemble_fresh(&assembly).unwrap().teardown().unwrap();
        let point = upper_point(upper.kind());
        for directory in ["upper", "work"] {
            let path = guest.path(&format!("run/nvx/{point}/{directory}"));
            assert_eq!(mode(&path), 0o755, "{directory}");
        }
        assert_eq!(
            guest.kernel.mounts(),
            below(&guest.text("run/nvx"), &["layers/distro", point, "rootfs"])
        );
    }

    // Empty directories that the scratch holds already pass, and keep their mode.
    let guest = Guest::new();
    for directory in ["upper", "work"] {
        let path = guest.path("run/nvx/scratch").join(directory);
        fs::create_dir_all(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    guest
        .assemble_fresh(&guest.assembly())
        .unwrap()
        .teardown()
        .unwrap();
    for directory in ["upper", "work"] {
        assert_eq!(mode(&guest.path("run/nvx/scratch").join(directory)), 0o700);
    }

    // A directory that holds an entry, such as the work directory that an earlier overlay
    // left, is refused once the upper is mounted, before the overlay, and before the other
    // directory is created; the failure unmounts the layer and the upper, and leaves the entry.
    for (directory, entry, other) in [("upper", "etc", "work"), ("work", "work", "upper")] {
        let guest = Guest::new();
        let path = guest.path("run/nvx/scratch").join(directory);
        fs::create_dir_all(path.join(entry)).unwrap();
        let error = guest.assemble_fresh(&guest.assembly()).unwrap_err();
        assert!(
            matches!(error.error(), Error::NotFresh { path: refused } if *refused == path),
            "{error:?}"
        );
        assert!(error.cleanup().is_none());
        assert_eq!(
            error.to_string(),
            format!(
                "the upper's directory {} is not empty, but the upper must be fresh",
                path.display()
            )
        );
        let mounted = below(&guest.text("run/nvx"), &["layers/distro", "scratch"]);
        assert_eq!(guest.kernel.mounts(), mounted);
        assert_eq!(guest.kernel.unmounts(), mounted);
        assert!(path.join(entry).is_dir());
        assert!(!guest.path("run/nvx/scratch").join(other).exists());
    }

    // So is anything there but a directory, a symbolic link to an empty one included, as any
    // root's assembly refuses it.
    for link in [false, true] {
        let guest = Guest::new();
        let upper = guest.path("run/nvx/scratch/upper");
        if link {
            fs::create_dir_all(guest.path("elsewhere")).unwrap();
            fs::create_dir_all(guest.path("run/nvx/scratch")).unwrap();
            std::os::unix::fs::symlink(guest.path("elsewhere"), &upper).unwrap();
        } else {
            write(&upper, "a file");
        }
        let error = guest.assemble_fresh(&guest.assembly()).unwrap_err();
        assert!(
            matches!(error.error(), Error::MountPoint { path, error }
                if *path == upper && error.raw_os_error() == Some(Errno::ENOTDIR as i32)),
            "{error:?}"
        );
    }

    // A cold root keeps both, whatever they hold, as a scratch keeps its upper across boots.
    let guest = Guest::new();
    for path in ["run/nvx/scratch/upper/etc", "run/nvx/scratch/work/work"] {
        fs::create_dir_all(guest.path(path)).unwrap();
    }
    guest
        .assemble(&guest.assembly())
        .unwrap()
        .teardown()
        .unwrap();
    assert!(guest.path("run/nvx/scratch/upper/etc").is_dir());
    assert!(guest.path("run/nvx/scratch/work/work").is_dir());
}

#[test]
fn the_freshness_check_takes_one_entry_of_a_directory_and_opens_none() {
    // However many entries a directory holds, the check refuses it at its first, and opens
    // none: not a FIFO, whose open would block, nor a directory below it.
    let guest = Guest::new();
    let upper = guest.path("run/nvx/scratch/upper");
    fs::create_dir_all(upper.join("a/b/c")).unwrap();
    for index in 0..1024 {
        fs::write(upper.join(format!("file{index}")), "content").unwrap();
    }
    let fifo = sys::path_c_string(&upper.join("fifo")).unwrap();
    sys::create_fifo(&fifo, sys::Mode::from_bits_truncate(0o600)).unwrap();
    let error = guest.assemble_fresh(&guest.assembly()).unwrap_err();
    assert!(
        matches!(error.error(), Error::NotFresh { path } if *path == upper),
        "{error:?}"
    );

    // The listing's first entry decides, and the check takes no other.
    let mut taken = 0;
    let listing = std::iter::from_fn(|| {
        taken += 1;
        Some(Ok::<_, io::Error>(()))
    });
    assert!(holds_entry(listing).unwrap());
    assert_eq!(taken, 1);
    assert!(!holds_entry(std::iter::empty::<io::Result<()>>()).unwrap());
    let failed = holds_entry(std::iter::once(Err::<(), _>(os_error(Errno::EIO))));
    assert_eq!(failed.unwrap_err().raw_os_error(), Some(Errno::EIO as i32));
}

#[test]
fn a_runtime_directory_that_another_root_holds_is_refused() {
    let guest = Guest::new();
    let first = guest.assemble(&guest.assembly()).unwrap();
    let calls = guest.kernel.calls();
    let error = guest.assemble(&guest.assembly()).unwrap_err();
    assert!(
        matches!(error.error(), Error::Reserved { path } if *path == guest.path("run/nvx")),
        "{error:?}"
    );
    assert!(error.cleanup().is_none());
    assert_eq!(
        error.to_string(),
        format!(
            "another root holds the runtime directory {}",
            guest.text("run/nvx")
        )
    );
    // The refused assembly touched none of the mounts, which the first root still owns.
    assert_eq!(guest.kernel.calls(), calls);
    first.teardown().unwrap();
    assert_eq!(
        guest.kernel.unmounts(),
        [
            guest.text("run/nvx/rootfs"),
            guest.text("run/nvx/layers/distro"),
            guest.text("run/nvx/scratch"),
        ]
    );
    // A root that is torn down or dropped holds the runtime directory no longer.
    drop(guest.assemble(&guest.assembly()).unwrap());
    guest
        .assemble(&guest.assembly())
        .unwrap()
        .teardown()
        .unwrap();

    // An assembly that starts while another one waits for its distro device is refused before
    // it checks or mounts anything, and the first one goes on.
    let guest = Guest::bare();
    attach(&guest.base, 1, "vdb", 8192);
    let refused = Arc::new(Mutex::new(None));
    let (kernel, paths, assembly) = (guest.kernel.clone(), guest.paths.clone(), guest.assembly());
    let (directory, outcome) = (guest.base.clone(), Arc::clone(&refused));
    guest.kernel.on_sleep(move |sleeps| {
        if sleeps == 1 {
            attach(&directory, 0, "vda", 4096);
            let calls = kernel.calls().len();
            let result = assemble_with(
                Box::new(kernel.clone()),
                &paths,
                &assembly,
                None,
                Contents::Kept,
            );
            *outcome.lock().unwrap() = Some((result.map(drop), kernel.calls().len() - calls));
        }
    });
    let mut assembly = guest.assembly();
    assembly.timeout = Duration::from_secs(1);
    let first = guest.assemble(&assembly).unwrap();
    let (result, calls) = refused.lock().unwrap().take().unwrap();
    let error = result.unwrap_err();
    assert!(
        matches!(error.error(), Error::Reserved { path } if *path == guest.path("run/nvx")),
        "{error:?}"
    );
    assert!(error.cleanup().is_none());
    assert_eq!(calls, 0);
    assert_eq!(
        guest.kernel.mounts(),
        [
            guest.text("run/nvx/layers/distro"),
            guest.text("run/nvx/scratch"),
            guest.text("run/nvx/rootfs"),
        ]
    );
    first.teardown().unwrap();
}

#[test]
fn a_runtime_directory_where_a_file_system_is_mounted_is_refused() {
    // A file system that no root mounted, at the distro layer's mount point.
    let guest = Guest::new();
    let distro = guest.path("run/nvx/layers/distro");
    guest
        .kernel
        .clone()
        .mount(
            Some(c"tmpfs"),
            &sys::path_c_string(&distro).unwrap(),
            Some(c"tmpfs"),
            MountFlags::empty(),
            None,
        )
        .unwrap();
    let calls = guest.kernel.calls();
    let error = guest.assemble(&guest.assembly()).unwrap_err();
    assert!(
        matches!(error.error(), Error::Occupied { path } if *path == distro),
        "{error:?}"
    );
    assert!(error.cleanup().is_none());
    assert_eq!(
        error.to_string(),
        format!("a file system is mounted at {} already", distro.display())
    );
    assert_eq!(guest.kernel.calls(), calls);

    // An overlay that a failed unmount left in place keeps the runtime directory occupied.
    let guest = Guest::new();
    guest.kernel.fail(
        Operation::Unmount,
        guest.path("run/nvx/rootfs"),
        Errno::EBUSY,
    );
    guest
        .assemble(&guest.assembly())
        .unwrap()
        .teardown()
        .unwrap_err();
    let mounts = guest.kernel.mounts().len();
    let error = guest.assemble(&guest.assembly()).unwrap_err();
    assert!(
        matches!(error.error(), Error::Occupied { path } if *path == guest.path("run/nvx/rootfs")),
        "{error:?}"
    );
    assert_eq!(guest.kernel.mounts().len(), mounts);

    // So does an aggregate share whose unmount alone failed: it is mounted outside the overlay,
    // which, like the image and the scratch, was unmounted.
    let guest = Guest::new();
    let mut root = guest.assemble(&guest.assembly()).unwrap();
    export(&guest, &[("a", ShareKind::Directory)]);
    let rw = Access::ReadWrite;
    root.mount_aggregate("all", rw, &[child("a", "/a", rw, ShareKind::Directory)])
        .unwrap();
    let shares = guest.path("run/nvx/shares");
    guest
        .kernel
        .fail(Operation::Unmount, shares.clone(), Errno::EBUSY);
    let error = root.teardown().unwrap_err();
    let failed: Vec<&Path> = error.failures().iter().map(Unmount::path).collect();
    assert_eq!(failed, [shares.as_path()]);
    let mounts = guest.kernel.mounts().len();
    let error = guest.assemble(&guest.assembly()).unwrap_err();
    assert!(
        matches!(error.error(), Error::Occupied { path } if *path == shares),
        "{error:?}"
    );
    assert_eq!(guest.kernel.mounts().len(), mounts);

    // So does a host export whose unmount alone failed, which is mounted outside the overlay too.
    let guest = Guest::new();
    let mut root = guest.assemble(&guest.assembly()).unwrap();
    drop(root.mount_export("microvm", Access::ReadOnly).unwrap());
    let mount_point = guest.path("run/nvx/hostfs/root");
    guest
        .kernel
        .fail(Operation::Unmount, mount_point.clone(), Errno::EBUSY);
    let error = root.teardown().unwrap_err();
    let failed: Vec<(&Mount, &Path)> = error
        .failures()
        .iter()
        .map(|failure| (failure.mount(), failure.path()))
        .collect();
    let mount = Mount::Export {
        tag: "microvm".into(),
    };
    assert_eq!(failed, [(&mount, mount_point.as_path())]);
    assert_eq!(
        error.to_string(),
        format!(
            "failed to unmount the host export microvm: {}",
            os_error(Errno::EBUSY)
        )
    );
    let mounts = guest.kernel.mounts().len();
    let error = guest.assemble(&guest.assembly()).unwrap_err();
    assert!(
        matches!(error.error(), Error::Occupied { path } if *path == mount_point),
        "{error:?}"
    );
    assert_eq!(guest.kernel.mounts().len(), mounts);

    // So does an upper that a failed unmount left, whichever upper the next root mounts.
    for (first, second) in [
        (scratch_upper(Journal::Allowed), Upper::Tmpfs),
        (Upper::Tmpfs, scratch_upper(Journal::Allowed)),
    ] {
        let guest = Guest::new();
        let left = guest.path(&format!("run/nvx/{}", upper_point(first.kind())));
        guest
            .kernel
            .fail(Operation::Unmount, left.clone(), Errno::EBUSY);
        let assembly = Assembly {
            upper: first,
            ..guest.assembly()
        };
        guest.assemble(&assembly).unwrap().teardown().unwrap_err();
        let mounts = guest.kernel.mounts().len();
        let calls = guest.kernel.calls();
        let assembly = Assembly {
            upper: second,
            ..guest.assembly()
        };
        let error = guest.assemble(&assembly).unwrap_err();
        assert!(
            matches!(error.error(), Error::Occupied { path } if *path == left),
            "{error:?}"
        );
        assert_eq!(guest.kernel.mounts().len(), mounts);
        assert_eq!(guest.kernel.calls(), calls);
    }
}

#[test]
fn the_kernel_finds_its_mount_points() {
    let directory = tempfile::tempdir().unwrap();
    let mut kernel = SystemKernel;
    assert!(kernel.is_mount_point(Path::new("/proc")).unwrap());
    assert!(!kernel.is_mount_point(directory.path()).unwrap());
    let error = kernel
        .is_mount_point(&directory.path().join("missing"))
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
}

#[test]
fn the_kernel_shows_a_mount_s_options() {
    let directory = tempfile::tempdir().unwrap();
    let mut kernel = SystemKernel;
    let options = kernel.mount_options(Path::new("/proc")).unwrap();
    let access = options.split(',').next().unwrap();
    assert!(access == "rw" || access == "ro", "{options}");
    let error = kernel
        .mount_options(&directory.path().join("missing"))
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
}

#[test]
#[ignore = "needs root"]
fn a_thread_with_private_tables_finds_its_mount_points_and_options() {
    std::thread::spawn(|| {
        sys::unshare_standard_descriptors().unwrap();
        sys::enter_private_mount_namespace().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let point = directory.path().join("mount");
        fs::create_dir(&point).unwrap();
        let mut kernel = SystemKernel;
        assert!(!kernel.is_mount_point(&point).unwrap());

        let path = sys::path_c_string(&point).unwrap();
        let mount = sys::TmpfsMount::mount(c"nvx-root-test", &path, c"size=64k").unwrap();
        assert!(kernel.is_mount_point(&point).unwrap());
        let child = point.join("child");
        fs::create_dir(&child).unwrap();
        assert!(!kernel.is_mount_point(&child).unwrap());
        let options = kernel.mount_options(&point).unwrap();
        assert!(options.split(',').any(|option| option == "rw"), "{options}");
        assert!(
            options.split(',').any(|option| option == "size=64k"),
            "{options}"
        );

        mount.unmount().unwrap();
        assert!(!kernel.is_mount_point(&point).unwrap());
    })
    .join()
    .unwrap();
}

#[test]
fn a_mount_s_options_are_the_last_field_of_its_line_in_the_mount_table() {
    let table = b"21 1 0:20 / /proc rw,nosuid,nodev,noexec,relatime shared:12 - proc proc rw\n\
                  35 21 8:1 / /run/a\\040b/rootfs rw,nosuid,nodev - overlay overlay \
                  rw,lowerdir=/a\\040b:/c,upperdir=/u,workdir=/w,metacopy=on\n\
                  36 35 0:5 / /mnt master:1 propagate_from:2 - tmpfs a\\040b ro,size=4k\n\
                  37 35 0:6 / /x rw\n";
    assert_eq!(super_options(table, 21), Some(&b"rw"[..]));
    assert_eq!(
        super_options(table, 35),
        Some(&b"rw,lowerdir=/a\\040b:/c,upperdir=/u,workdir=/w,metacopy=on"[..])
    );
    assert_eq!(super_options(table, 36), Some(&b"ro,size=4k"[..]));
    // A mount without a line, and a line without the separator.
    assert_eq!(super_options(table, 3), None);
    assert_eq!(super_options(table, 37), None);
}

/// The variable that gives the bind-mount test's inner run the runtime directory where the outer
/// run bound a directory at the distro layer's mount point.
const BOUND_RUNTIME: &str = "NVX_ROOT_TEST_BOUND_RUNTIME";

#[test]
fn a_runtime_directory_where_a_directory_is_bound_is_refused() {
    // A directory of the same file system, bound at the distro layer's mount point, shows the
    // device of the directory's parent, so only their mounts tell it apart. The outer run binds
    // it in a user and mount namespace of its own, where it runs the test again, which assembles
    // a root there with the kernel itself.
    if let Some(runtime) = std::env::var_os(BOUND_RUNTIME) {
        let runtime = PathBuf::from(runtime);
        let distro = runtime.join(DISTRO_DIRECTORY);
        let device = |path: &Path| fs::metadata(path).unwrap().dev();
        assert_eq!(device(&distro), device(distro.parent().unwrap()));
        let guest = Guest::new();
        let assembly = Assembly {
            runtime,
            ..guest.assembly()
        };
        let error = assemble_with(
            Box::new(SystemKernel),
            &guest.paths,
            &assembly,
            None,
            Contents::Kept,
        )
        .unwrap_err();
        assert!(
            matches!(error.error(), Error::Occupied { path } if *path == distro),
            "{error:?}"
        );
        assert!(error.cleanup().is_none());
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let base = directory.path().canonicalize().unwrap();
    let source = base.join("source");
    let runtime = base.join("run/nvx");
    let distro = runtime.join(DISTRO_DIRECTORY);
    fs::create_dir(&source).unwrap();
    fs::create_dir_all(&distro).unwrap();
    let test = "guest::root::tests::a_runtime_directory_where_a_directory_is_bound_is_refused";
    let output = std::process::Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount", "--propagation"])
        .args(["private", "sh", "-c"])
        .arg(r#"mount --bind "$1" "$2" || exit 77; shift 2; exec "$@""#)
        .arg("sh")
        .args([&source, &distro])
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(BOUND_RUNTIME, &runtime)
        .output();
    let output = match output {
        Ok(output) => output,
        // Without `unshare`, the test stops here.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => panic!("cannot run unshare: {error}"),
    };
    // So it does where the namespaces cannot be created, which fails `unshare` with 1, or where
    // they cannot bind a directory, which the script reports with 77.
    if matches!(output.status.code(), Some(1 | 77)) {
        return;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("test result: ok. 1 passed"),
        "{}\n{stdout}{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Returns the overlay's options for the layers at `lowers` below the runtime directory
/// `runtime`, the top first, over the upper mounted at `upper` below it, with `mode`.
fn overlay_data(runtime: &str, lowers: &[&str], upper: &str, mode: OverlayMode) -> String {
    let lowers: Vec<String> = lowers
        .iter()
        .map(|lower| format!("{runtime}/{lower}"))
        .collect();
    format!(
        "lowerdir={},upperdir={runtime}/{upper}/upper,workdir={runtime}/{upper}/work,{}",
        lowers.join(":"),
        mode.options()
    )
}

/// Returns the paths of `names` below the runtime directory `runtime`, as the fake kernel
/// records them.
fn below(runtime: &str, names: &[&str]) -> Vec<String> {
    names
        .iter()
        .map(|name| format!("{runtime}/{name}"))
        .collect()
}

#[test]
fn a_gpt_disk_s_layers_are_mounted_under_the_overlay_top_first() {
    let guest = Guest::new();
    guest.insert(&GptDisk::new(b"{\"configuration\": true}\n", 3));
    let assembly = guest.gpt_assembly(scratch_upper(Journal::Allowed), OverlayMode::TrustedLowers);
    let root = guest.assemble(&assembly).unwrap();
    let runtime = guest.text("run/nvx");
    let layer = |number: u8| {
        mount_call(
            Some(&guest.text(&format!("dev/vda{number}"))),
            &format!("{runtime}/layers/p{number}"),
            Some("ext4"),
            MountFlags::MS_RDONLY | MountFlags::MS_NOSUID | MountFlags::MS_NODEV,
            Some("noload,errors=remount-ro"),
        )
    };
    // The disk is flushed, then its table read and checked, and the scratch checked, before any
    // mount; the overlay stacks the layers with partition 2's at its base and partition 4's at
    // its top.
    let options = overlay_data(
        &runtime,
        &["layers/p4", "layers/p3", "layers/p2"],
        "scratch",
        OverlayMode::TrustedLowers,
    );
    assert_eq!(
        guest.kernel.calls(),
        [
            Call::Flush(guest.text("dev/vda")),
            Call::ReadDisk(guest.text("dev/vda")),
            Call::Flush(guest.text("dev/vdb")),
            layer(2),
            layer(3),
            layer(4),
            mount_call(
                Some(&guest.text("dev/vdb")),
                &format!("{runtime}/scratch"),
                Some("ext4"),
                MountFlags::MS_NOSUID | MountFlags::MS_NODEV,
                None,
            ),
            mount_call(
                Some("overlay"),
                &format!("{runtime}/rootfs"),
                Some("overlay"),
                MountFlags::MS_NOSUID | MountFlags::MS_NODEV,
                Some(&options),
            ),
        ]
    );
    // The first partition comes back as its bytes, without the padding after them, and is never
    // mounted.
    assert_eq!(
        root.first_partition(),
        Some(&b"{\"configuration\": true}"[..])
    );
    let numbers: Vec<u8> = root
        .lowers()
        .iter()
        .map(|lower| lower.partition().unwrap().number())
        .collect();
    assert_eq!(numbers, [2, 3, 4]);
    for lower in root.lowers() {
        let partition = lower.partition().unwrap();
        let number = partition.number();
        assert_eq!(lower.mount(), &Mount::Layer { number });
        assert_eq!(lower.device(), guest.path(&format!("dev/vda{number}")));
        assert_eq!(
            lower.path(),
            guest.path(&format!("run/nvx/layers/p{number}"))
        );
        assert_eq!(partition.unique_guid(), GptDisk::guid(number));
    }
    assert_eq!(root.upper(), UpperKind::Scratch);
    assert_eq!(root.overlay(), OverlayMode::TrustedLowers);
    root.teardown().unwrap();
    assert_eq!(
        guest.kernel.unmounts(),
        below(
            &runtime,
            &["rootfs", "layers/p4", "layers/p3", "layers/p2", "scratch"]
        )
    );

    // A layer's mount point where a file system is mounted already is refused before any
    // mount.
    let guest = Guest::new();
    guest.insert(&GptDisk::new(b"configuration", 3));
    let third = guest.path("run/nvx/layers/p3");
    fs::create_dir_all(&third).unwrap();
    guest
        .kernel
        .clone()
        .mount(
            Some(c"tmpfs"),
            &sys::path_c_string(&third).unwrap(),
            Some(c"tmpfs"),
            MountFlags::empty(),
            None,
        )
        .unwrap();
    let mounts = guest.kernel.mounts();
    let assembly = guest.gpt_assembly(scratch_upper(Journal::Allowed), OverlayMode::TrustedLowers);
    let error = guest.assemble(&assembly).unwrap_err();
    assert!(
        matches!(error.error(), Error::Occupied { path } if *path == third),
        "{error:?}"
    );
    assert_eq!(guest.kernel.mounts(), mounts);
}

#[test]
fn both_overlay_modes_assemble_on_both_uppers_with_their_own_option_sets() {
    let modes = [OverlayMode::TrustedLowers, OverlayMode::UntrustedLowers];
    for gpt in [false, true] {
        for upper in [scratch_upper(Journal::Allowed), Upper::Tmpfs] {
            for mode in modes {
                let case = format!("GPT {gpt}, {upper:?}, {mode:?}");
                let guest = Guest::new();
                let mut assembly = Assembly {
                    upper,
                    overlay: mode,
                    ..guest.assembly()
                };
                let lowers: &[&str] = if gpt {
                    guest.insert(&GptDisk::new(b"configuration", 2));
                    assembly.image = Image::Gpt {
                        expected: Expected::new(),
                        first_partition_bound: FIRST_PARTITION_BOUND,
                    };
                    &["layers/p3", "layers/p2"]
                } else {
                    &["layers/distro"]
                };
                let root = guest.assemble(&assembly).unwrap();
                let runtime = guest.text("run/nvx");
                let flags = MountFlags::MS_NOSUID | MountFlags::MS_NODEV;
                let (point, upper_mount) = match upper.kind() {
                    UpperKind::Scratch => (
                        "scratch",
                        mount_call(
                            Some(&guest.text("dev/vdb")),
                            &format!("{runtime}/scratch"),
                            Some("ext4"),
                            flags,
                            None,
                        ),
                    ),
                    UpperKind::Tmpfs => (
                        "tmpfs",
                        mount_call(
                            Some("tmpfs"),
                            &format!("{runtime}/tmpfs"),
                            Some("tmpfs"),
                            flags,
                            Some("mode=0755"),
                        ),
                    ),
                };
                // The upper, then the overlay, with the mode's one option set, and the flags
                // `nosuid,nodev`.
                let overlay = mount_call(
                    Some("overlay"),
                    &format!("{runtime}/rootfs"),
                    Some("overlay"),
                    flags,
                    Some(&overlay_data(&runtime, lowers, point, mode)),
                );
                let calls = guest.kernel.calls();
                assert_eq!(calls[calls.len() - 2..], [upper_mount, overlay], "{case}");
                // A tmpfs upper looks for no scratch device.
                let scratch_flushed = calls.contains(&Call::Flush(guest.text("dev/vdb")));
                assert_eq!(
                    scratch_flushed,
                    upper.kind() == UpperKind::Scratch,
                    "{case}"
                );
                assert_eq!(root.upper(), upper.kind(), "{case}");
                assert_eq!(root.overlay(), mode, "{case}");
                assert_eq!(root.first_partition().is_some(), gpt, "{case}");
                for directory in ["upper", "work"] {
                    let path = guest.path(&format!("run/nvx/{point}/{directory}"));
                    assert!(path.is_dir(), "{case}: {directory}");
                }
                root.teardown().unwrap();
                let unmounts: Vec<&str> = ["rootfs"]
                    .into_iter()
                    .chain(lowers.iter().copied())
                    .chain([point])
                    .collect();
                assert_eq!(
                    guest.kernel.unmounts(),
                    below(&runtime, &unmounts),
                    "{case}"
                );
            }
        }
    }
}

#[test]
fn a_tmpfs_upper_needs_no_scratch_device() {
    let guest = Guest::bare();
    attach(&guest.base, 0, "vda", 4096);
    let assembly = Assembly {
        upper: Upper::Tmpfs,
        ..guest.assembly()
    };
    let root = guest.assemble(&assembly).unwrap();
    // The assembly waited for no other device, and made no mount point for a scratch.
    assert_eq!(guest.kernel.sleeps(), 0);
    assert!(!guest.path("run/nvx/scratch").exists());
    root.teardown().unwrap();
    assert_eq!(
        guest.kernel.unmounts(),
        below(
            &guest.text("run/nvx"),
            &["rootfs", "layers/distro", "tmpfs"]
        )
    );
}

#[test]
fn a_refused_option_set_fails_without_a_fallback() {
    for mode in [OverlayMode::TrustedLowers, OverlayMode::UntrustedLowers] {
        for upper in [scratch_upper(Journal::Allowed), Upper::Tmpfs] {
            let guest = Guest::new();
            guest.kernel.fail(
                Operation::Mount,
                guest.path("run/nvx/rootfs"),
                Errno::EINVAL,
            );
            let assembly = Assembly {
                upper,
                overlay: mode,
                ..guest.assembly()
            };
            let error = guest.assemble(&assembly).unwrap_err();
            assert!(
                matches!(
                    error.error(),
                    Error::Mount { mount: Mount::Overlay, error }
                        if error.raw_os_error() == Some(Errno::EINVAL as i32)
                ),
                "{error:?}"
            );
            assert!(error.cleanup().is_none());
            // The overlay was mounted once, with the mode's own option set.
            let overlays: Vec<Call> = guest
                .kernel
                .calls()
                .into_iter()
                .filter(|call| matches!(call, Call::Mount { target, .. } if target.ends_with("/rootfs")))
                .collect();
            let [
                Call::Mount {
                    data: Some(data), ..
                },
            ] = overlays.as_slice()
            else {
                panic!("{overlays:?}");
            };
            assert!(data.ends_with(&format!(",{}", mode.options())), "{data}");
            // The layer and the upper are unmounted again.
            let point = match upper.kind() {
                UpperKind::Scratch => "scratch",
                UpperKind::Tmpfs => "tmpfs",
            };
            assert_eq!(
                guest.kernel.unmounts(),
                below(&guest.text("run/nvx"), &["layers/distro", point])
            );
        }
    }
}

#[test]
fn an_overlay_that_the_kernel_mounts_without_its_mode_in_effect_is_refused_and_unmounted() {
    // Without failing the mount, the kernel drops `metacopy=on` for an upper that can't hold the
    // overlay's extended attributes. Its mount table then shows no `metacopy`, which is off by
    // default.
    for upper in [scratch_upper(Journal::Allowed), Upper::Tmpfs] {
        let guest = Guest::new();
        guest.show_overlay("rw", upper.kind(), "xino=on");
        let assembly = Assembly {
            upper,
            ..guest.assembly()
        };
        let error = guest.assemble(&assembly).unwrap_err();
        assert!(
            matches!(error.error(), Error::OverlayFallback { requested, effective }
                if requested == "metacopy=on" && effective == "metacopy=off"),
            "{error:?}"
        );
        assert_eq!(
            error.error().to_string(),
            "the kernel mounted the overlay with metacopy=off in place of metacopy=on"
        );
        assert!(error.cleanup().is_none());
        assert_eq!(
            guest.kernel.unmounts(),
            below(
                &guest.text("run/nvx"),
                &["rootfs", "layers/distro", upper_point(upper.kind())]
            )
        );
    }

    // It mounts an overlay read-only when it can't create the overlay's work directory, as on a
    // full upper.
    for mode in [OverlayMode::TrustedLowers, OverlayMode::UntrustedLowers] {
        let guest = Guest::new();
        guest.show_overlay("ro", UpperKind::Scratch, mode.options());
        let assembly = Assembly {
            overlay: mode,
            ..guest.assembly()
        };
        let error = guest.assemble(&assembly).unwrap_err();
        assert!(
            matches!(error.error(), Error::OverlayFallback { requested, effective }
                if requested == "rw" && effective == "ro"),
            "{error:?}"
        );
        assert_eq!(
            guest.kernel.unmounts(),
            below(
                &guest.text("run/nvx"),
                &["rootfs", "layers/distro", "scratch"]
            )
        );
    }
}

#[test]
fn options_that_the_kernel_does_not_show_have_their_defaults() {
    // The kernel's mount table shows no option at its default, as for each option of the
    // untrusted lowers under the guest's kernel, whose defaults are off.
    let untrusted = |guest: &Guest| Assembly {
        upper: Upper::Tmpfs,
        overlay: OverlayMode::UntrustedLowers,
        ..guest.assembly()
    };
    let guest = Guest::new();
    guest.show_overlay("rw", UpperKind::Tmpfs, "");
    let root = guest.assemble(&untrusted(&guest)).unwrap();
    assert_eq!(root.overlay(), OverlayMode::UntrustedLowers);
    root.teardown().unwrap();

    // Under a kernel that copies up metadata alone by default, the table shows the trusted
    // lowers' `metacopy=on` no more, and must show the untrusted lowers' `metacopy=off`.
    let guest = Guest::new();
    guest.set_overlay_default("metacopy", true);
    guest.show_overlay("rw", UpperKind::Scratch, "redirect_dir=on,xino=on");
    guest
        .assemble(&guest.assembly())
        .unwrap()
        .teardown()
        .unwrap();
    let guest = Guest::new();
    guest.set_overlay_default("metacopy", true);
    guest.show_overlay("rw", UpperKind::Tmpfs, "");
    let error = guest.assemble(&untrusted(&guest)).unwrap_err();
    assert!(
        matches!(error.error(), Error::OverlayFallback { requested, effective }
            if requested == "metacopy=off" && effective == "metacopy=on"),
        "{error:?}"
    );

    // Under a kernel that creates or follows redirects by default, the table must show
    // `redirect_dir=nofollow`.
    for (parameter, effective) in [("redirect_dir", "on"), ("redirect_always_follow", "follow")] {
        let guest = Guest::new();
        guest.set_overlay_default(parameter, true);
        guest.show_overlay("rw", UpperKind::Tmpfs, "");
        let error = guest.assemble(&untrusted(&guest)).unwrap_err();
        assert!(
            matches!(error.error(), Error::OverlayFallback { requested, effective: actual }
                if requested == "redirect_dir=nofollow"
                    && *actual == format!("redirect_dir={effective}")),
            "{parameter}: {error:?}"
        );
        let guest = Guest::new();
        guest.set_overlay_default(parameter, true);
        guest.show_overlay("rw", UpperKind::Tmpfs, "redirect_dir=nofollow");
        guest
            .assemble(&untrusted(&guest))
            .unwrap()
            .teardown()
            .unwrap();
    }
}

#[test]
fn effective_options_that_cannot_be_read_fail_the_assembly() {
    // The overlay's options in the kernel's mount table.
    let guest = Guest::new();
    let rootfs = guest.path("run/nvx/rootfs");
    guest
        .kernel
        .fail(Operation::Options, rootfs.clone(), Errno::EIO);
    let error = guest.assemble(&guest.assembly()).unwrap_err();
    assert!(
        matches!(error.error(), Error::OverlayCheck { path, error }
            if *path == rootfs && error.raw_os_error() == Some(Errno::EIO as i32)),
        "{error:?}"
    );
    assert_eq!(
        error.error().to_string(),
        format!(
            "failed to read the overlay's effective options at {}: {}",
            rootfs.display(),
            os_error(Errno::EIO)
        )
    );
    assert!(error.cleanup().is_none());
    assert_eq!(
        guest.kernel.unmounts(),
        below(
            &guest.text("run/nvx"),
            &["rootfs", "layers/distro", "scratch"]
        )
    );

    // The default of an option that the table doesn't show: missing, or neither `Y` nor `N`.
    for contents in [None, Some("1\n")] {
        let guest = Guest::new();
        let parameter = guest.paths.sys.join(OVERLAY_PARAMETERS).join("metacopy");
        match contents {
            Some(contents) => write(&parameter, contents),
            None => fs::remove_file(&parameter).unwrap(),
        }
        guest.show_overlay("rw", UpperKind::Tmpfs, "");
        let assembly = Assembly {
            upper: Upper::Tmpfs,
            overlay: OverlayMode::UntrustedLowers,
            ..guest.assembly()
        };
        let error = guest.assemble(&assembly).unwrap_err();
        let expected = match contents {
            Some(_) => Some(Errno::EPROTO as i32),
            None => Some(Errno::ENOENT as i32),
        };
        assert!(
            matches!(error.error(), Error::OverlayCheck { path, error }
                if *path == parameter && error.raw_os_error() == expected),
            "{error:?}"
        );
        assert_eq!(guest.kernel.unmounts().len(), 3);
    }
}

#[test]
fn overlay_options_that_a_mount_cannot_pass_are_refused_before_any_mount() {
    // "lowerdir=/L,upperdir=/u,workdir=/w,metacopy=on,xino=on" takes 53 bytes and the length of
    // the lower's name L: options of the limit's length pass, and longer ones do not.
    let options = |name: usize| {
        let lower = Lower {
            mount: Mount::Distro,
            device: PathBuf::new(),
            path: format!("/{}", "l".repeat(name)).into(),
            partition: None,
        };
        let mode = OverlayMode::TrustedLowers;
        overlay_options(&[lower], Path::new("/u"), Path::new("/w"), mode)
    };
    assert_eq!(
        options(MOUNT_OPTIONS_LIMIT - 53).unwrap().len(),
        MOUNT_OPTIONS_LIMIT
    );
    assert!(matches!(
        options(MOUNT_OPTIONS_LIMIT - 52),
        Err(Error::OverlayOptions { length: 4096 })
    ));

    // A runtime directory long enough that the options, which name it three times, take more
    // than a mount passes is refused before any mount.
    let guest = Guest::new();
    let component = "d".repeat(250);
    let runtime = guest.path(&format!("run/{}/nvx", [component.as_str(); 6].join("/")));
    let assembly = Assembly {
        runtime: runtime.clone(),
        upper: Upper::Tmpfs,
        ..guest.assembly()
    };
    let error = guest.assemble(&assembly).unwrap_err();
    let runtime = runtime.display().to_string();
    let mode = OverlayMode::TrustedLowers;
    let length = overlay_data(&runtime, &["layers/distro"], "tmpfs", mode).len();
    assert!(length > MOUNT_OPTIONS_LIMIT, "{length}");
    assert!(
        matches!(error.error(), Error::OverlayOptions { length: actual } if *actual == length),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "the overlay's options take {length} bytes, more than the 4095 that a mount passes"
        )
    );
    assert!(guest.kernel.mounts().is_empty());
}

#[test]
fn up_to_195_layers_fit_in_the_overlay_s_options_below_run_nvx() {
    // Below `/run/nvx`, the options of 195 layers, those of partitions 2 through 196, fit
    // whatever the mode and the upper, and those of 196 do not.
    let lower = |number: u8| Lower {
        mount: Mount::Layer { number },
        device: PathBuf::new(),
        path: format!("/run/nvx/layers/p{number}").into(),
        partition: None,
    };
    for point in ["scratch", "tmpfs"] {
        for mode in [OverlayMode::TrustedLowers, OverlayMode::UntrustedLowers] {
            let upper = Path::new("/run/nvx").join(point);
            let options = |layers: u8| {
                let lowers: Vec<Lower> = (2..=layers + 1).map(lower).collect();
                overlay_options(&lowers, &upper.join("upper"), &upper.join("work"), mode)
            };
            let longest = options(195).unwrap();
            assert!(longest.len() <= MOUNT_OPTIONS_LIMIT, "{}", longest.len());
            assert!(
                matches!(options(196), Err(Error::OverlayOptions { length })
                    if length > MOUNT_OPTIONS_LIMIT),
                "{point}, {mode:?}"
            );
        }
    }

    // An assembly of a disk with more layers refuses their options before it mounts anything.
    let guest = Guest::new();
    guest.insert(&GptDisk::new(b"configuration", 254));
    let mode = OverlayMode::UntrustedLowers;
    let error = guest
        .assemble(&guest.gpt_assembly(Upper::Tmpfs, mode))
        .unwrap_err();
    let layers: Vec<String> = (2..=255)
        .rev()
        .map(|number| format!("layers/p{number}"))
        .collect();
    let layers: Vec<&str> = layers.iter().map(String::as_str).collect();
    let length = overlay_data(&guest.text("run/nvx"), &layers, "tmpfs", mode).len();
    assert!(
        matches!(error.error(), Error::OverlayOptions { length: actual } if *actual == length),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "the overlay's options take {length} bytes, more than the 4095 that a mount passes"
        )
    );
    assert!(guest.kernel.mounts().is_empty());
}

#[test]
fn the_journal_policy_is_checked_in_the_scratch_s_superblock_before_any_mount() {
    // A scratch without a journal mounts whatever the policy, and one with a journal only where
    // the policy allows one.
    let cases = [
        (false, Journal::Allowed, true),
        (false, Journal::Forbidden, true),
        (true, Journal::Allowed, true),
        (true, Journal::Forbidden, false),
    ];
    for (journal, policy, mounts) in cases {
        let guest = Guest::new();
        fs::write(guest.path("dev/vdb"), ext4_image(journal)).unwrap();
        let assembly = Assembly {
            upper: scratch_upper(policy),
            ..guest.assembly()
        };
        let result = guest.assemble(&assembly);
        if mounts {
            result.unwrap().teardown().unwrap();
            continue;
        }
        let error = result.unwrap_err();
        assert!(
            matches!(error.error(), Error::Journal { device } if *device == guest.path("dev/vdb")),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!(
                "the scratch device {} has an ext4 journal, which the assembly forbids",
                guest.text("dev/vdb")
            )
        );
        assert!(guest.kernel.mounts().is_empty());
    }

    // A scratch that holds no ext4 file system has no journal feature to check.
    let guest = Guest::new();
    fs::write(guest.path("dev/vdb"), vec![0; 2048]).unwrap();
    let assembly = Assembly {
        upper: scratch_upper(Journal::Forbidden),
        ..guest.assembly()
    };
    let error = guest.assemble(&assembly).unwrap_err();
    assert!(
        matches!(error.error(), Error::NotExt4 { device } if *device == guest.path("dev/vdb")),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "the scratch device {} holds no ext4 file system",
            guest.text("dev/vdb")
        )
    );
    assert!(guest.kernel.mounts().is_empty());
}

#[test]
fn a_device_whose_access_mode_is_not_its_role_s_is_refused_before_it_is_flushed() {
    // A writable distro device.
    let guest = Guest::new();
    set_read_only(&guest.base, "vda", false);
    let error = guest
        .assemble(&guest.assembly())
        .unwrap_err()
        .into_parts()
        .0;
    assert!(
        matches!(&error, Error::Access { role: Role::Distro, device, read_only: false }
            if *device == guest.path("dev/vda")),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "the distro device {} is writable, expected read-only",
            guest.text("dev/vda")
        )
    );
    assert!(guest.kernel.calls().is_empty());

    // A read-only scratch device, checked after the distro device.
    let guest = Guest::new();
    set_read_only(&guest.base, "vdb", true);
    let error = guest
        .assemble(&guest.assembly())
        .unwrap_err()
        .into_parts()
        .0;
    assert!(
        matches!(&error, Error::Access { role: Role::Scratch, device, read_only: true }
            if *device == guest.path("dev/vdb")),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "the scratch device {} is read-only, expected writable",
            guest.text("dev/vdb")
        )
    );
    assert_eq!(guest.kernel.calls(), [Call::Flush(guest.text("dev/vda"))]);

    // The devices in each other's roles: the distro's role names the scratch's window.
    let guest = Guest::new();
    let mut assembly = guest.assembly();
    assembly.distro.mmio = SCRATCH;
    scratch(&mut assembly).mmio = DISTRO;
    let error = guest.assemble(&assembly).unwrap_err().into_parts().0;
    assert!(
        matches!(&error, Error::Access { role: Role::Distro, device, read_only: false }
            if *device == guest.path("dev/vdb")),
        "{error:?}"
    );
    assert!(guest.kernel.calls().is_empty());

    // An access mode in another form than the kernel's.
    let guest = Guest::new();
    write(&guest.path("sys/class/block/vda/ro"), "2\n");
    let error = guest
        .assemble(&guest.assembly())
        .unwrap_err()
        .into_parts()
        .0;
    assert!(
        matches!(&error, Error::Device { role: Role::Distro, step: DeviceStep::Access, error, .. }
            if error.raw_os_error() == Some(Errno::EPROTO as i32)),
        "{error:?}"
    );
    assert!(guest.kernel.calls().is_empty());
}

#[test]
fn a_tmpfs_upper_is_unmounted_last_and_every_failure_reported() {
    let guest = Guest::new();
    let assembly = Assembly {
        upper: Upper::Tmpfs,
        overlay: OverlayMode::UntrustedLowers,
        ..guest.assembly()
    };
    let root = guest.assemble(&assembly).unwrap();
    for name in ["run/nvx/layers/distro", "run/nvx/tmpfs"] {
        guest
            .kernel
            .fail(Operation::Unmount, guest.path(name), Errno::EBUSY);
    }
    let error = root.teardown().unwrap_err();
    assert_eq!(
        guest.kernel.unmounts(),
        below(
            &guest.text("run/nvx"),
            &["rootfs", "layers/distro", "tmpfs"]
        )
    );
    let failed: Vec<(&Mount, &Path)> = error
        .failures()
        .iter()
        .map(|failure| (failure.mount(), failure.path()))
        .collect();
    let distro = guest.path("run/nvx/layers/distro");
    let tmpfs = guest.path("run/nvx/tmpfs");
    assert_eq!(
        failed,
        [
            (&Mount::Distro, distro.as_path()),
            (&Mount::Tmpfs, tmpfs.as_path())
        ]
    );
    assert_eq!(
        error.to_string(),
        format!(
            "failed to unmount the distro layer: {busy}; failed to unmount the tmpfs upper: {busy}",
            busy = os_error(Errno::EBUSY)
        )
    );
}

#[test]
fn a_scratch_s_file_system_is_synced_right_before_its_unmount() {
    // A copy of the root's mounts in another mount namespace keeps the scratch mounted, and its
    // writes unsynced, after the teardown unmounts it here, so the teardown syncs it first.
    let guest = Guest::new();
    let root = guest.assemble(&guest.assembly()).unwrap();
    let assembled = guest.kernel.calls().len();
    root.teardown().unwrap();
    let runtime = guest.text("run/nvx");
    let expected = [
        Call::Unmount(format!("{runtime}/rootfs")),
        Call::Unmount(format!("{runtime}/layers/distro")),
        Call::SyncFileSystem(format!("{runtime}/scratch")),
        Call::Unmount(format!("{runtime}/scratch")),
    ];
    assert_eq!(&guest.kernel.calls()[assembled..], expected.as_slice());

    // A tmpfs upper has no device to sync to.
    let guest = Guest::new();
    let assembly = Assembly {
        upper: Upper::Tmpfs,
        ..guest.assembly()
    };
    let root = guest.assemble(&assembly).unwrap();
    let assembled = guest.kernel.calls().len();
    root.teardown().unwrap();
    let expected: Vec<Call> = below(
        &guest.text("run/nvx"),
        &["rootfs", "layers/distro", "tmpfs"],
    )
    .into_iter()
    .map(Call::Unmount)
    .collect();
    assert_eq!(&guest.kernel.calls()[assembled..], expected.as_slice());
}

#[test]
fn a_scratch_whose_sync_fails_stays_mounted_and_the_failure_is_reported() {
    let guest = Guest::new();
    let root = guest.assemble(&guest.assembly()).unwrap();
    let scratch = guest.path("run/nvx/scratch");
    guest
        .kernel
        .fail(Operation::SyncFileSystem, scratch.clone(), Errno::EIO);
    let error = root.teardown().unwrap_err();
    // The overlay and the layer go, and the scratch, whose writes may not be on its device,
    // stays.
    assert_eq!(
        guest.kernel.unmounts(),
        below(&guest.text("run/nvx"), &["rootfs", "layers/distro"])
    );
    assert!(guest.kernel.mount_at(&scratch).is_some());
    let failed: Vec<(&Mount, &Path)> = error
        .failures()
        .iter()
        .map(|failure| (failure.mount(), failure.path()))
        .collect();
    assert_eq!(failed, [(&Mount::Scratch, scratch.as_path())]);
    assert_eq!(
        error.failures()[0].error().raw_os_error(),
        Some(Errno::EIO as i32)
    );
    assert_eq!(
        error.to_string(),
        format!("failed to sync the scratch: {}", os_error(Errno::EIO))
    );
}

#[test]
fn a_gpt_disk_that_differs_from_the_expected_identities_is_refused_before_any_mount() {
    let disk = GptDisk::new(b"configuration", 2);
    let partition = |number: u8| {
        ExpectedPartition::new()
            .unique_guid(GptDisk::guid(number))
            .size(disk.size(number))
    };
    // The disk meets every expectation that the caller may set.
    let guest = Guest::new();
    guest.insert(&disk);
    let mut assembly = guest.gpt_assembly(Upper::Tmpfs, OverlayMode::UntrustedLowers);
    assembly.image = Image::Gpt {
        expected: Expected::new()
            .disk_guid(DISK_GUID)
            .partitions((1..=3).map(partition).collect()),
        first_partition_bound: FIRST_PARTITION_BOUND,
    };
    guest.assemble(&assembly).unwrap().teardown().unwrap();

    let other = Uuid::from_bytes([0x5A; 16]);
    let cases = [
        (
            Expected::new().disk_guid(other),
            GptError::DiskGuidMismatch {
                expected: other,
                found: DISK_GUID,
            },
        ),
        (
            Expected::new().partitions(vec![partition(1), partition(2)]),
            GptError::PartitionCountMismatch {
                expected: 2,
                found: 3,
            },
        ),
        (
            Expected::new().partitions(vec![
                partition(1),
                partition(2),
                ExpectedPartition::new().unique_guid(other),
            ]),
            GptError::PartitionGuidMismatch {
                number: 3,
                expected: other,
                found: GptDisk::guid(3),
            },
        ),
        (
            Expected::new().partitions(vec![
                partition(1),
                ExpectedPartition::new().size(2 * BLOCK as u64),
                partition(3),
            ]),
            GptError::PartitionSizeMismatch {
                number: 2,
                expected: 2 * BLOCK as u64,
                found: (LAYER_BLOCKS * BLOCK) as u64,
            },
        ),
    ];
    for (expected, failure) in cases {
        let guest = Guest::new();
        guest.insert(&disk);
        let mut assembly = guest.gpt_assembly(Upper::Tmpfs, OverlayMode::UntrustedLowers);
        assembly.image = Image::Gpt {
            expected,
            first_partition_bound: FIRST_PARTITION_BOUND,
        };
        let error = guest.assemble(&assembly).unwrap_err();
        assert!(
            matches!(error.error(), Error::Disk { device, error: DeviceError::Table(_) }
                if *device == guest.path("dev/vda")),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!(
                "the GPT disk on the distro device {} failed its validation: invalid partition \
                 table: {failure}",
                guest.text("dev/vda")
            )
        );
        assert!(guest.kernel.mounts().is_empty());
    }
}

#[test]
fn a_gpt_disk_without_a_layer_is_refused_before_any_mount() {
    let guest = Guest::new();
    guest.insert(&GptDisk::new(b"configuration", 0));
    let assembly = guest.gpt_assembly(Upper::Tmpfs, OverlayMode::TrustedLowers);
    let error = guest.assemble(&assembly).unwrap_err();
    assert!(
        matches!(
            error.error(),
            Error::Disk {
                error: DeviceError::Table(GptError::PartitionCount(1)),
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "the GPT disk on the distro device {} failed its validation: invalid partition \
             table: the disk has 1 partitions, not 2 through 255: the first partition and at \
             least one layer",
            guest.text("dev/vda")
        )
    );
    assert!(guest.kernel.mounts().is_empty());
}

#[test]
fn a_layer_that_is_not_ext4_or_whose_journal_needs_a_recovery_is_refused_before_any_mount() {
    // A layer may have a journal.
    let mut disk = GptDisk::new(b"configuration", 3);
    disk.layer_mut(2)[1024 + 0x5C] |= 0x04;
    let guest = Guest::new();
    guest.insert(&disk);
    let assembly = guest.gpt_assembly(Upper::Tmpfs, OverlayMode::TrustedLowers);
    guest.assemble(&assembly).unwrap().teardown().unwrap();

    // But not one that needs recovery, despite our `noload` or the superblock's own defaults.
    let layer = disk.layer_mut(3);
    layer[1024 + 0x60] |= 0x04;
    layer[1024 + 0x200..][..10].copy_from_slice(b"norecovery");
    let guest = Guest::new();
    guest.insert(&disk);
    let assembly = guest.gpt_assembly(Upper::Tmpfs, OverlayMode::TrustedLowers);
    let error = guest.assemble(&assembly).unwrap_err();
    assert!(
        matches!(error.error(), Error::Recovery { number: 3, device }
            if *device == guest.path("dev/vda3")),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "the layer in partition 3, {}, needs a journal recovery, which its read-only device \
             can't take",
            guest.text("dev/vda3")
        )
    );
    assert!(guest.kernel.mounts().is_empty());

    // Nor one that holds no ext4 file system.
    let mut disk = GptDisk::new(b"configuration", 2);
    disk.layer_mut(2)[1024 + 0x38] ^= 1;
    let guest = Guest::new();
    guest.insert(&disk);
    let assembly = guest.gpt_assembly(Upper::Tmpfs, OverlayMode::TrustedLowers);
    let error = guest.assemble(&assembly).unwrap_err();
    assert!(
        matches!(error.error(), Error::NotExt4Layer { number: 2, device }
            if *device == guest.path("dev/vda2")),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "the distro's partition 2, {}, holds no ext4 file system",
            guest.text("dev/vda2")
        )
    );
    assert!(guest.kernel.mounts().is_empty());

    // Nor one whose superblock can't be read.
    let disk = GptDisk::new(b"configuration", 1);
    let guest = Guest::new();
    fs::write(guest.path("dev/vda"), disk.image()).unwrap();
    fs::create_dir(guest.path("dev/vda2")).unwrap();
    let assembly = guest.gpt_assembly(Upper::Tmpfs, OverlayMode::TrustedLowers);
    let error = guest.assemble(&assembly).unwrap_err();
    assert!(
        matches!(
            error.error(),
            Error::Device {
                role: Role::Distro,
                step: DeviceStep::Superblock,
                device,
                error,
            } if *device == guest.path("dev/vda2")
                && error.raw_os_error() == Some(Errno::EISDIR as i32)
        ),
        "{error:?}"
    );
    assert!(guest.kernel.mounts().is_empty());
}

#[test]
fn a_missing_partition_fails_at_once_before_any_mount() {
    // The kernel created the partitions' nodes when it found the disk, so the assembly awaits
    // none: a partition whose node is missing fails without a wait.
    let disk = GptDisk::new(b"configuration", 2);
    let guest = Guest::new();
    fs::write(guest.path("dev/vda"), disk.image()).unwrap();
    write(&guest.path("dev/vda2"), disk.layer(2));
    let assembly = guest.gpt_assembly(Upper::Tmpfs, OverlayMode::TrustedLowers);
    let error = guest.assemble(&assembly).unwrap_err();
    assert!(
        matches!(error.error(), Error::Partition { number: 3, node }
            if *node == guest.path("dev/vda3")),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "the distro's partition 3 has no block device at {}",
            guest.text("dev/vda3")
        )
    );
    assert_eq!(guest.kernel.sleeps(), 0);
    assert!(guest.kernel.mounts().is_empty());
}

#[test]
fn a_reread_asks_the_kernel_for_the_distro_disk_s_table_once_the_device_appears() {
    // The reread finds the distro device by its window, as an assembly does, and only asks the
    // kernel to reread the disk's table: it checks, flushes, reads, and mounts nothing.
    let guest = Guest::new();
    let distro = guest.assembly().distro;
    let timeout = Duration::from_millis(100);
    let node = block::reread(
        &mut guest.kernel.clone(),
        &guest.paths,
        distro,
        timeout,
        None,
    )
    .unwrap();
    assert_eq!(node, guest.path("dev/vda"));
    assert_eq!(guest.kernel.calls(), [Call::Reread(guest.text("dev/vda"))]);

    // It waits for the device as long as the timeout allows, and asks nothing of one that does
    // not appear.
    let guest = Guest::bare();
    let directory = guest.base.clone();
    guest.kernel.on_sleep(move |sleeps| {
        if sleeps == 1 {
            attach(&directory, 0, "vda", 4096);
        }
    });
    let waiting = Duration::from_secs(1);
    block::reread(
        &mut guest.kernel.clone(),
        &guest.paths,
        distro,
        waiting,
        None,
    )
    .unwrap();
    assert_eq!(guest.kernel.sleeps(), 1);
    assert_eq!(guest.kernel.calls(), [Call::Reread(guest.text("dev/vda"))]);
    let guest = Guest::bare();
    let error = block::reread(
        &mut guest.kernel.clone(),
        &guest.paths,
        distro,
        timeout,
        None,
    )
    .unwrap_err();
    assert!(
        matches!(
            error,
            Error::Missing {
                role: Role::Distro,
                mmio: DISTRO,
            }
        ),
        "{error:?}"
    );
    assert_eq!(guest.kernel.sleeps(), 2);
    assert!(guest.kernel.calls().is_empty());

    // However long its timeout, one whose deadline passed looks for the device once, without
    // sleeping, as an assembly does, and asks nothing of a missing one.
    let guest = Guest::bare();
    let patient = Duration::from_secs(600);
    let error = block::reread(
        &mut guest.kernel.clone(),
        &guest.paths,
        distro,
        patient,
        Some(Instant::now()),
    )
    .unwrap_err();
    assert!(
        matches!(
            error,
            Error::Missing {
                role: Role::Distro,
                mmio: DISTRO,
            }
        ),
        "{error:?}"
    );
    assert_eq!(guest.kernel.sleeps(), 0);
    assert!(guest.kernel.calls().is_empty());

    // The kernel's refusal, as while one of the disk's partitions is open, names the device.
    let guest = Guest::new();
    let vda = guest.path("dev/vda");
    guest.kernel.fail(Operation::Reread, &vda, Errno::EBUSY);
    let error = block::reread(
        &mut guest.kernel.clone(),
        &guest.paths,
        distro,
        timeout,
        None,
    )
    .unwrap_err();
    assert!(
        matches!(
            &error,
            Error::Device {
                role: Role::Distro,
                step: DeviceStep::Reread,
                device,
                error,
            } if *device == vda && error.raw_os_error() == Some(Errno::EBUSY as i32)
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "failed to reread the partition table of the distro device {}: {}",
            vda.display(),
            os_error(Errno::EBUSY)
        )
    );
}

/// The loop device of a test, which it detaches once dropped.
struct LoopDevice(PathBuf);

impl LoopDevice {
    /// Attaches the disk image `image`, read-only and with its partitions scanned, as the kernel
    /// scans those of a virtio disk, or returns `None` without `losetup`.
    fn attach(image: &Path) -> Option<Self> {
        let output = std::process::Command::new("losetup")
            .args(["--find", "--show", "--read-only", "--partscan"])
            .arg(image)
            .output();
        let output = match output {
            Ok(output) => output,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
            Err(error) => panic!("cannot run losetup: {error}"),
        };
        assert!(output.status.success(), "{output:?}");
        let node = String::from_utf8(output.stdout).unwrap();
        Some(Self(PathBuf::from(node.trim_end())))
    }

    /// Returns the number, start, and size, in 512-byte sectors, of each partition that the
    /// kernel created, as sysfs lists them, in order.
    fn partitions(&self) -> Vec<(u64, u64, u64)> {
        let name = self.0.file_name().unwrap().to_str().unwrap();
        let disk = Path::new("/sys/class/block").join(name);
        let mut partitions = Vec::new();
        for entry in fs::read_dir(&disk).unwrap() {
            let entry = entry.unwrap().path();
            if !entry.join("partition").exists() {
                continue;
            }
            let number = |file: &str| {
                let text = fs::read_to_string(entry.join(file)).unwrap();
                text.trim_end().parse::<u64>().unwrap()
            };
            partitions.push((number("partition"), number("start"), number("size")));
        }
        partitions.sort_unstable();
        partitions
    }

    /// Returns the node of partition `number`.
    fn partition(&self, number: u8) -> PathBuf {
        let mut node = self.0.clone().into_os_string();
        node.push(format!("p{number}"));
        node.into()
    }
}

impl Drop for LoopDevice {
    fn drop(&mut self) {
        let _ = std::process::Command::new("losetup")
            .arg("--detach")
            .arg(&self.0)
            .status();
    }
}

/// Waits until udev, if the host runs it, handled its events, during which its probes may hold
/// a partition open.
fn settle() {
    let _ = std::process::Command::new("udevadm").arg("settle").status();
}

/// Writes `message` to the standard error, past the test harness's capture, as the guest's
/// programs write theirs.
fn report(message: &str) {
    use std::io::Write;

    let _ = writeln!(io::stderr(), "{message}");
}

#[test]
#[ignore = "needs root and a loop device"]
fn the_kernel_rereads_a_partition_table_that_changed_under_it() {
    // A read-only loop device stands for a clone's distro device: the kernel scanned it when it
    // found the template's placeholder, and the disk now holds the clone's, of another layout.
    if !nix::unistd::geteuid().is_root() {
        report("skipped: the test needs root");
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let image = directory.path().join("disk");
    let placeholder = GptDisk::new(b"placeholder", 3).image();
    let clone = GptDisk::new(&[0x5a; 600], 1).image();
    let length = placeholder.len().max(clone.len());
    let padded = |disk: &[u8]| {
        let mut disk = disk.to_vec();
        disk.resize(length, 0);
        disk
    };
    fs::write(&image, padded(&placeholder)).unwrap();
    let Some(device) = LoopDevice::attach(&image) else {
        report("skipped: the test needs losetup");
        return;
    };
    settle();
    // Both disks' tables take 34 blocks: the MBR, the header, and 128 entries of 128 bytes. The
    // placeholder's first partition takes one block and the clone's two, then each layer four.
    let found = [(1, 34, 1), (2, 35, 4), (3, 39, 4), (4, 43, 4)];
    assert_eq!(device.partitions(), found);
    fs::write(&image, padded(&clone)).unwrap();
    assert_eq!(device.partitions(), found);

    // The reread drops the placeholder's partitions, and creates the clone's, with their nodes,
    // from the disk's bytes, before it returns.
    let node = sys::path_c_string(&device.0).unwrap();
    SystemKernel.reread_partitions(&node).unwrap();
    assert_eq!(device.partitions(), [(1, 34, 2), (2, 36, 4)]);
    assert!(device.partition(2).exists());
    assert!(!device.partition(3).exists());

    // While one of the disk's partitions is open, the kernel refuses to reread its table.
    settle();
    let open = fs::File::open(device.partition(2)).unwrap();
    assert_eq!(SystemKernel.reread_partitions(&node), Err(Errno::EBUSY));
    drop(open);
}

#[test]
fn the_first_partition_is_returned_as_opaque_bytes_within_its_bound() {
    // One whole sector of bytes that are not UTF-8, with no padding.
    let sector: Vec<u8> = (0..BLOCK).map(|index| 0x80 | index as u8).collect();
    assert!(std::str::from_utf8(&sector).is_err());
    // Trailing NUL bytes and ASCII whitespace are padding, and the others are the first
    // partition's bytes.
    let padded = b"\0 key=value\0\t\n \r\x0c\0\0";
    let cases: [(&[u8], &[u8]); 3] = [
        (&sector, &sector),
        (padded, b"\0 key=value"),
        (b"\n\0", b""),
    ];
    for (bytes, expected) in cases {
        let guest = Guest::new();
        let disk = GptDisk::new(bytes, 1);
        guest.insert(&disk);
        let mut assembly = guest.gpt_assembly(Upper::Tmpfs, OverlayMode::TrustedLowers);
        // The first partition may be as large as the bound.
        assembly.image = Image::Gpt {
            expected: Expected::new(),
            first_partition_bound: disk.first_partition_blocks() * BLOCK,
        };
        let root = guest.assemble(&assembly).unwrap();
        assert_eq!(root.first_partition(), Some(expected));
        assert_eq!(
            guest.kernel.mounts(),
            below(&guest.text("run/nvx"), &["layers/p2", "tmpfs", "rootfs"])
        );
        root.teardown().unwrap();
    }

    // A first partition larger than the bound is refused before any mount.
    let guest = Guest::new();
    guest.insert(&GptDisk::new(&sector, 1));
    let mut assembly = guest.gpt_assembly(Upper::Tmpfs, OverlayMode::TrustedLowers);
    assembly.image = Image::Gpt {
        expected: Expected::new(),
        first_partition_bound: BLOCK - 1,
    };
    let error = guest.assemble(&assembly).unwrap_err();
    assert!(
        matches!(
            error.error(),
            Error::FirstPartition {
                device,
                error: GptError::FirstPartitionTooLarge {
                    size: 512,
                    bound: 511
                },
            } if *device == guest.path("dev/vda")
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "failed to read the first partition of the GPT disk on the distro device {}: the \
             first partition has 512 bytes, more than the bound of 511",
            guest.text("dev/vda")
        )
    );
    assert!(guest.kernel.mounts().is_empty());
}

#[test]
fn a_teardown_unmounts_the_layers_top_first_and_reports_every_failure() {
    let guest = Guest::new();
    guest.insert(&GptDisk::new(b"configuration", 3));
    let assembly = guest.gpt_assembly(Upper::Tmpfs, OverlayMode::UntrustedLowers);
    let root = guest.assemble(&assembly).unwrap();
    for name in ["run/nvx/layers/p3", "run/nvx/tmpfs"] {
        guest
            .kernel
            .fail(Operation::Unmount, guest.path(name), Errno::EBUSY);
    }
    let error = root.teardown().unwrap_err();
    let runtime = guest.text("run/nvx");
    assert_eq!(
        guest.kernel.unmounts(),
        below(
            &runtime,
            &["rootfs", "layers/p4", "layers/p3", "layers/p2", "tmpfs"]
        )
    );
    let failed: Vec<(&Mount, &Path)> = error
        .failures()
        .iter()
        .map(|failure| (failure.mount(), failure.path()))
        .collect();
    let (third, tmpfs) = (guest.path("run/nvx/layers/p3"), guest.path("run/nvx/tmpfs"));
    assert_eq!(
        failed,
        [
            (&Mount::Layer { number: 3 }, third.as_path()),
            (&Mount::Tmpfs, tmpfs.as_path())
        ]
    );
    assert_eq!(
        error.to_string(),
        format!(
            "failed to unmount the layer in partition 3: {}; failed to unmount the tmpfs upper: {}",
            os_error(Errno::EBUSY),
            os_error(Errno::EBUSY)
        )
    );

    // A layer that fails to mount leaves the layers below it to unmount, and nothing above.
    let guest = Guest::new();
    guest.insert(&GptDisk::new(b"configuration", 3));
    guest.kernel.fail(
        Operation::Mount,
        guest.path("run/nvx/layers/p3"),
        Errno::EINVAL,
    );
    let assembly = guest.gpt_assembly(Upper::Tmpfs, OverlayMode::UntrustedLowers);
    let error = guest.assemble(&assembly).unwrap_err();
    assert!(
        matches!(
            error.error(),
            Error::Mount {
                mount: Mount::Layer { number: 3 },
                ..
            }
        ),
        "{error:?}"
    );
    assert!(error.cleanup().is_none());
    assert_eq!(guest.kernel.unmounts(), [guest.text("run/nvx/layers/p2")]);
}

#[test]
fn a_layer_that_a_failed_unmount_left_keeps_the_runtime_directory_occupied() {
    // A GPT disk's top layer, left mounted, refuses a root of an EROFS image, and one of a disk
    // with fewer layers, which mounts nothing there itself.
    let guest = Guest::new();
    guest.insert(&GptDisk::new(b"configuration", 3));
    let top = guest.path("run/nvx/layers/p4");
    guest
        .kernel
        .fail(Operation::Unmount, top.clone(), Errno::EBUSY);
    let assembly = guest.gpt_assembly(Upper::Tmpfs, OverlayMode::TrustedLowers);
    guest.assemble(&assembly).unwrap().teardown().unwrap_err();
    guest.insert(&GptDisk::new(b"configuration", 1));
    let (mounts, calls) = (guest.kernel.mounts().len(), guest.kernel.calls());
    let erofs = Assembly {
        upper: Upper::Tmpfs,
        ..guest.assembly()
    };
    for assembly in [erofs, assembly] {
        let error = guest.assemble(&assembly).unwrap_err();
        assert!(
            matches!(error.error(), Error::Occupied { path } if *path == top),
            "{error:?}"
        );
        assert_eq!(guest.kernel.mounts().len(), mounts);
        assert_eq!(guest.kernel.calls(), calls);
    }

    // An EROFS image, left mounted, refuses a root of a GPT disk.
    let guest = Guest::new();
    let distro = guest.path("run/nvx/layers/distro");
    guest
        .kernel
        .fail(Operation::Unmount, distro.clone(), Errno::EBUSY);
    let assembly = Assembly {
        upper: Upper::Tmpfs,
        ..guest.assembly()
    };
    guest.assemble(&assembly).unwrap().teardown().unwrap_err();
    guest.insert(&GptDisk::new(b"configuration", 1));
    let (mounts, calls) = (guest.kernel.mounts().len(), guest.kernel.calls());
    let assembly = guest.gpt_assembly(Upper::Tmpfs, OverlayMode::TrustedLowers);
    let error = guest.assemble(&assembly).unwrap_err();
    assert!(
        matches!(error.error(), Error::Occupied { path } if *path == distro),
        "{error:?}"
    );
    assert_eq!(guest.kernel.mounts().len(), mounts);
    assert_eq!(guest.kernel.calls(), calls);
}

/// Returns a child of an aggregate share.
fn child(name: &str, target: &str, access: Access, kind: ShareKind) -> ShareChild {
    ShareChild {
        name: name.to_owned(),
        target: target.into(),
        access,
        kind,
    }
}

/// Returns the absolute targets of the live shares and mappings that `root`'s mapper tracks.
fn targets(root: &Root) -> Vec<String> {
    root.mapper
        .iter()
        .flat_map(Mapper::targets)
        .map(|target| format!("/{}", target.display()))
        .collect()
}

/// Creates the entries that an aggregate mounted at `shares` in `guest`'s runtime directory
/// shows, which the fake kernel does not mount: a directory or a regular file for each child.
fn export(guest: &Guest, children: &[(&str, ShareKind)]) {
    let shares = guest.path("run/nvx/shares");
    for &(name, kind) in children {
        match kind {
            ShareKind::Directory => fs::create_dir_all(shares.join(name)).unwrap(),
            ShareKind::File => write(&shares.join(name), "host file"),
        }
    }
}

#[test]
fn what_a_share_s_target_lacks_is_created_with_its_mode_whatever_the_umask_or_a_default_acl() {
    // A workload that is not root must be able to enter the directories that the root creates
    // for a share, whatever the umask of the process that mounts it, or a default ACL of the
    // overlay's root, which withholds permissions in the umask's stead.
    for restricted in [false, true] {
        sys::with_thread_umask(0o077, || {
            let guest = Guest::new();
            let mut root = guest.assemble(&guest.assembly()).unwrap();
            if restricted && !sys::restrict_new_entries(&guest.path("run/nvx/rootfs")) {
                root.teardown().unwrap();
                return;
            }
            export(&guest, &[("b", ShareKind::File)]);
            root.mount_share("one", Path::new("/srv/new/share"), Access::ReadOnly)
                .unwrap();
            root.mount_aggregate(
                "all",
                Access::ReadOnly,
                &[child(
                    "b",
                    "/opt/app/config",
                    Access::ReadOnly,
                    ShareKind::File,
                )],
            )
            .unwrap();
            let mode = |relative: &str| fs::metadata(guest.path(relative)).unwrap().mode() & 0o7777;
            for (relative, expected) in [
                ("run/nvx/rootfs/srv", 0o755),
                ("run/nvx/rootfs/srv/new", 0o755),
                ("run/nvx/rootfs/srv/new/share", 0o755),
                ("run/nvx/rootfs/opt", 0o755),
                ("run/nvx/rootfs/opt/app", 0o755),
                ("run/nvx/rootfs/opt/app/config", 0o644),
            ] {
                assert_eq!(mode(relative), expected, "{relative} {restricted}");
            }
            assert_eq!(sys::thread_umask(), 0o077);
            root.teardown().unwrap();
        });
    }
}

#[test]
fn shares_are_mounted_restricted_and_torn_down_first_in_reverse_order() {
    let guest = Guest::new();
    let mut root = guest.assemble(&guest.assembly()).unwrap();
    let assembled = guest.kernel.calls().len();
    export(
        &guest,
        &[("a", ShareKind::Directory), ("b", ShareKind::File)],
    );
    root.mount_share("one", Path::new("/srv/one"), Access::ReadOnly)
        .unwrap();
    root.mount_aggregate(
        "all",
        Access::ReadWrite,
        &[
            child("a", "/data/a", Access::ReadWrite, ShareKind::Directory),
            child("b", "/etc/app/config", Access::ReadOnly, ShareKind::File),
        ],
    )
    .unwrap();
    let rootfs = guest.text("run/nvx/rootfs");
    let shares = guest.text("run/nvx/shares");
    let staging = guest.text("run/nvx/staging");
    let restricted = MountFlags::MS_NOSUID | MountFlags::MS_NODEV;
    // The share's device is mounted out of the root with its flags, then moved onto its target;
    // each child is a copy of its entry's mount, restricted before it is attached.
    let mut expected = vec![
        mount_call(
            Some("one"),
            &staging,
            Some("virtiofs"),
            restricted | MountFlags::MS_RDONLY,
            None,
        ),
        Call::OpenTree {
            source: staging.clone(),
            clone: false,
        },
        Call::SetAttributes {
            source: staging.clone(),
            set: MountAttributes::READ_ONLY
                | MountAttributes::NO_SET_ID
                | MountAttributes::NO_DEVICES,
            clear: MountAttributes::empty(),
        },
        Call::MoveMount {
            source: staging.clone(),
            target: format!("{rootfs}/srv/one"),
        },
        mount_call(Some("all"), &shares, Some("virtiofs"), restricted, None),
    ];
    expected.extend(attach_calls(
        &guest.path("run/nvx/shares/a"),
        &guest.path("run/nvx/rootfs/data/a"),
        Access::ReadWrite,
    ));
    expected.extend(attach_calls(
        &guest.path("run/nvx/shares/b"),
        &guest.path("run/nvx/rootfs/etc/app/config"),
        Access::ReadOnly,
    ));
    assert_eq!(guest.kernel.calls()[assembled..], expected);
    let mode = |relative: &str| fs::metadata(guest.path(relative)).unwrap().mode() & 0o7777;
    assert!(guest.path("run/nvx/rootfs/srv/one").is_dir());
    assert!(guest.path("run/nvx/rootfs/etc/app/config").is_file());
    assert_eq!(mode("run/nvx/rootfs/etc/app"), 0o755);
    assert_eq!(mode("run/nvx/rootfs/etc/app/config"), 0o644);
    assert_eq!(guest.kernel.mount_at(&guest.path("run/nvx/staging")), None);
    assert!(!root.is_latched());

    root.teardown().unwrap();
    assert_eq!(
        guest.kernel.unmounts(),
        [
            format!("{rootfs}/etc/app/config"),
            format!("{rootfs}/data/a"),
            format!("{rootfs}/srv/one"),
            shares,
            rootfs,
            guest.text("run/nvx/layers/distro"),
            guest.text("run/nvx/scratch"),
        ]
    );
}

#[test]
fn refused_shares_mount_nothing() {
    let guest = Guest::new();
    let mut root = guest.assemble(&guest.assembly()).unwrap();
    root.mount_share("srv", Path::new("/srv"), Access::ReadWrite)
        .unwrap();
    let mounts = guest.kernel.mounts().len();
    let refusal = |result: Result<(), Error>| match result {
        Err(Error::Share(refusal)) => refusal,
        other => panic!("{other:?}"),
    };
    let directory = ShareKind::Directory;
    let rw = Access::ReadWrite;
    assert_eq!(
        refusal(root.mount_share("a b", Path::new("/a"), rw)),
        ShareRefusal::Tag("a b".into())
    );
    assert_eq!(
        refusal(root.mount_share("t", Path::new("/proc/x"), rw)),
        ShareRefusal::Reserved("/proc/x".into())
    );
    assert_eq!(
        refusal(root.mount_share("t", Path::new("/"), rw)),
        ShareRefusal::NotCanonical("/".into())
    );
    assert_eq!(
        refusal(root.mount_share("t", Path::new("/srv/inner"), rw)),
        ShareRefusal::Overlap {
            claimed: "/srv".into(),
            target: "/srv/inner".into()
        }
    );
    assert_eq!(
        refusal(root.mount_aggregate("t", rw, &[])),
        ShareRefusal::NoChildren
    );
    assert_eq!(
        refusal(root.mount_aggregate("t", rw, &[child("..", "/a", rw, directory)])),
        ShareRefusal::ChildName("..".into())
    );
    assert_eq!(
        refusal(root.mount_aggregate(
            "t",
            rw,
            &[
                child("x", "/a", rw, directory),
                child("x", "/b", rw, directory)
            ]
        )),
        ShareRefusal::DuplicateChild("x".into())
    );
    assert_eq!(
        refusal(root.mount_aggregate(
            "t",
            rw,
            &[
                child("x", "/b/c", rw, directory),
                child("y", "/b", rw, directory)
            ]
        )),
        ShareRefusal::Overlap {
            claimed: "/b/c".into(),
            target: "/b".into()
        }
    );
    assert_eq!(
        refusal(root.mount_aggregate("t", rw, &[child("x", "/", rw, directory)])),
        ShareRefusal::NotCanonical("/".into())
    );
    assert_eq!(guest.kernel.mounts().len(), mounts);
    // The refusals of requests latch nothing.
    assert!(!root.is_latched());

    // A share that fails to mount claims no target, and latches the root.
    guest.kernel.fail(
        Operation::Mount,
        guest.path("run/nvx/staging"),
        Errno::ENODEV,
    );
    let error = root.mount_share("opt", Path::new("/opt"), rw).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "failed to mount the live share opt at /opt: {}",
            os_error(Errno::ENODEV)
        )
    );
    assert_eq!(targets(&root), ["/srv"]);
    assert!(root.is_latched());
    let error = root.mount_share("t", Path::new("/t"), rw).unwrap_err();
    assert!(
        matches!(
            &error,
            Error::Mapping { mount: Mount::Share { tag, .. }, error: MappingError::Latched }
                if tag == "t"
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "failed to mount the live share t at /t: an earlier mapping failed"
    );
}

#[test]
fn a_root_has_one_aggregate_and_one_that_failed_part_of_the_way_latches_it() {
    let rw = Access::ReadWrite;
    let directory = ShareKind::Directory;
    let rootfs = |guest: &Guest, target: &str| guest.text(&format!("run/nvx/rootfs{target}"));
    for bound in [&["/a", "/b"][..], &["/a"]] {
        let guest = Guest::new();
        let mut root = guest.assemble(&guest.assembly()).unwrap();
        export(&guest, &[("a", directory), ("b", directory)]);
        if bound.len() == 1 {
            guest.kernel.fail(
                Operation::MoveMount,
                guest.path("run/nvx/rootfs/b"),
                Errno::ENOENT,
            );
        }
        let first = root.mount_aggregate(
            "all",
            rw,
            &[
                child("a", "/a", rw, directory),
                child("b", "/b", rw, directory),
            ],
        );
        assert_eq!(first.is_ok(), bound.len() == 2, "{first:?}");
        assert_eq!(root.is_latched(), bound.len() == 1);
        let mounts = guest.kernel.mounts().len();
        let error = root
            .mount_aggregate("more", rw, &[child("c", "/c", rw, directory)])
            .unwrap_err();
        if bound.len() == 2 {
            assert!(
                matches!(error, Error::Share(ShareRefusal::SecondAggregate)),
                "{error:?}"
            );
            assert_eq!(
                error.to_string(),
                "the root has an aggregate live share already"
            );
        } else {
            assert!(
                matches!(
                    error,
                    Error::Mapping {
                        error: MappingError::Latched,
                        ..
                    }
                ),
                "{error:?}"
            );
        }
        assert_eq!(guest.kernel.mounts().len(), mounts);
        assert!(!guest.path("run/nvx/rootfs/c").exists());
        // The teardown unmounts the bound children, then the one aggregate.
        root.teardown().unwrap();
        let mut unmounts: Vec<String> = bound
            .iter()
            .rev()
            .map(|target| rootfs(&guest, target))
            .collect();
        unmounts.extend([
            guest.text("run/nvx/shares"),
            guest.text("run/nvx/rootfs"),
            guest.text("run/nvx/layers/distro"),
            guest.text("run/nvx/scratch"),
        ]);
        assert_eq!(guest.kernel.unmounts(), unmounts);
    }
}

#[test]
fn a_child_whose_restriction_fails_is_never_attached() {
    let guest = Guest::new();
    let mut root = guest.assemble(&guest.assembly()).unwrap();
    export(
        &guest,
        &[("a", ShareKind::Directory), ("b", ShareKind::Directory)],
    );
    guest.kernel.fail(
        Operation::SetAttributes,
        guest.path("run/nvx/shares/a"),
        Errno::EPERM,
    );
    let error = root
        .mount_aggregate(
            "all",
            Access::ReadOnly,
            &[
                child("a", "/data/a", Access::ReadOnly, ShareKind::Directory),
                child("b", "/data/b", Access::ReadOnly, ShareKind::Directory),
            ],
        )
        .unwrap_err();
    assert!(
        matches!(
            &error,
            Error::Mapping { mount: Mount::Child { name, target }, error }
                if name == "a" && *target == Path::new("/data/a")
                    && error.raw_os_error() == Errno::EPERM as i32
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "failed to mount the live share child a at /data/a: failed to restrict the mount: {}",
            os_error(Errno::EPERM)
        )
    );
    assert!(guest.kernel.moves().is_empty());
    root.teardown().unwrap();
    assert_eq!(
        guest.kernel.unmounts(),
        [
            guest.text("run/nvx/shares"),
            guest.text("run/nvx/rootfs"),
            guest.text("run/nvx/layers/distro"),
            guest.text("run/nvx/scratch"),
        ]
    );
}

#[test]
fn a_child_that_the_aggregate_lacks_or_holds_as_another_kind_binds_nothing() {
    for (exported, errno, cause) in [
        (
            None,
            Errno::ENOENT,
            format!("failed to open the source: {}", os_error(Errno::ENOENT)),
        ),
        (
            Some(ShareKind::File),
            Errno::EINVAL,
            "the source is not a directory or a regular file of the mapping's kind".to_owned(),
        ),
    ] {
        let guest = Guest::new();
        let mut root = guest.assemble(&guest.assembly()).unwrap();
        if let Some(kind) = exported {
            export(&guest, &[("a", kind)]);
        }
        let mounts = guest.kernel.mounts().len();
        let error = root
            .mount_aggregate(
                "all",
                Access::ReadOnly,
                &[child(
                    "a",
                    "/data/a",
                    Access::ReadOnly,
                    ShareKind::Directory,
                )],
            )
            .unwrap_err();
        assert!(
            matches!(
                &error,
                Error::Mapping { mount: Mount::Child { name, .. }, error }
                    if name == "a" && error.raw_os_error() == errno as i32
            ),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!("failed to mount the live share child a at /data/a: {cause}")
        );
        // Only the aggregate is mounted, and the child claims no target.
        assert_eq!(
            guest.kernel.mounts()[mounts..],
            [guest.text("run/nvx/shares")]
        );
        assert!(targets(&root).is_empty());
        assert!(!guest.path("run/nvx/rootfs/data").exists());
        root.teardown().unwrap();
        assert_eq!(guest.kernel.unmounts()[0], guest.text("run/nvx/shares"));
    }
}

#[test]
fn a_failed_unmount_is_reported_and_the_later_ones_still_attempted() {
    let guest = Guest::new();
    let mut root = guest.assemble(&guest.assembly()).unwrap();
    root.mount_share("srv", Path::new("/srv"), Access::ReadWrite)
        .unwrap();
    guest.kernel.fail(
        Operation::Unmount,
        guest.path("run/nvx/rootfs/srv"),
        Errno::EBUSY,
    );
    guest.kernel.fail(
        Operation::Unmount,
        guest.path("run/nvx/rootfs"),
        Errno::EBUSY,
    );
    let error = root.teardown().unwrap_err();
    let failures: Vec<_> = error
        .failures()
        .iter()
        .map(|failure| (failure.mount().clone(), failure.path().to_owned()))
        .collect();
    assert_eq!(
        failures,
        [
            (
                Mount::Share {
                    tag: "srv".into(),
                    target: "/srv".into()
                },
                guest.path("run/nvx/rootfs/srv")
            ),
            (Mount::Overlay, guest.path("run/nvx/rootfs")),
        ]
    );
    assert_eq!(
        error.to_string(),
        format!(
            "failed to unmount the live share srv at /srv: {busy}; \
             failed to unmount the overlay: {busy}",
            busy = os_error(Errno::EBUSY)
        )
    );
    assert_eq!(
        guest.kernel.unmounts(),
        [
            guest.text("run/nvx/rootfs/srv"),
            guest.text("run/nvx/rootfs"),
            guest.text("run/nvx/layers/distro"),
            guest.text("run/nvx/scratch"),
        ]
    );
}

#[test]
fn a_share_through_a_link_or_onto_another_mount_is_refused_and_latches_the_root() {
    let rw = Access::ReadWrite;
    // A link in the root, which the mapper's walk refuses after the device was mounted at
    // `staging`, which it unmounts then.
    let guest = Guest::new();
    let mut root = guest.assemble(&guest.assembly()).unwrap();
    fs::create_dir(guest.path("elsewhere")).unwrap();
    std::os::unix::fs::symlink(guest.path("elsewhere"), guest.path("run/nvx/rootfs/link")).unwrap();
    let error = root
        .mount_share("t", Path::new("/link/new"), rw)
        .unwrap_err();
    assert!(
        matches!(
            &error,
            Error::Mapping { mount: Mount::Share { tag, .. }, error: MappingError::Link { path } }
                if tag == "t" && *path == Path::new("/link")
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "failed to mount the live share t at /link/new: /link is a symbolic link or a magic link"
    );
    assert_eq!(guest.kernel.unmounts(), [guest.text("run/nvx/staging")]);
    assert!(!guest.path("elsewhere/new").exists());
    assert!(root.is_latched());
    root.teardown().unwrap();

    // Another mount in the root, where nothing is created.
    let guest = Guest::new();
    let mut root = guest.assemble(&guest.assembly()).unwrap();
    let foreign = guest.path("run/nvx/rootfs/foreign");
    fs::create_dir(&foreign).unwrap();
    guest.kernel.add_mount(&foreign);
    let error = root
        .mount_share("t", Path::new("/foreign/new"), rw)
        .unwrap_err();
    assert!(
        matches!(
            &error,
            Error::Mapping { error: MappingError::Foreign { path }, .. }
                if *path == Path::new("/foreign/new")
        ),
        "{error:?}"
    );
    assert!(!foreign.join("new").exists());

    // A device that stays at `staging`, because its unmount failed too, is the teardown's.
    let guest = Guest::new();
    let mut root = guest.assemble(&guest.assembly()).unwrap();
    std::os::unix::fs::symlink("/", guest.path("run/nvx/rootfs/link")).unwrap();
    let staging = guest.path("run/nvx/staging");
    guest
        .kernel
        .fail(Operation::Unmount, staging.clone(), Errno::EBUSY);
    root.mount_share("t", Path::new("/link/new"), rw)
        .unwrap_err();
    let error = root.teardown().unwrap_err();
    let failed: Vec<_> = error
        .failures()
        .iter()
        .map(|failure| (failure.mount().clone(), failure.path().to_owned()))
        .collect();
    assert_eq!(
        failed,
        [(
            Mount::Share {
                tag: "t".into(),
                target: "/link/new".into()
            },
            staging.clone()
        )]
    );

    // A `staging` where a file system is mounted already is refused before the device mounts.
    let guest = Guest::new();
    let mut root = guest.assemble(&guest.assembly()).unwrap();
    let staging = guest.path("run/nvx/staging");
    fs::create_dir(&staging).unwrap();
    guest.kernel.add_mount(&staging);
    let mounts = guest.kernel.mounts().len();
    let error = root.mount_share("t", Path::new("/t"), rw).unwrap_err();
    assert!(
        matches!(&error, Error::Occupied { path } if *path == staging),
        "{error:?}"
    );
    assert_eq!(guest.kernel.mounts().len(), mounts);
}

#[test]
fn a_share_or_a_mapping_that_a_rename_moved_and_that_stays_is_the_teardown_s() {
    for (share, detach) in [
        (true, Ok(())),
        (true, Err(Errno::EBUSY)),
        (false, Err(Errno::EBUSY)),
    ] {
        let guest = Guest::new();
        let mut root = guest.assemble(&guest.assembly()).unwrap();
        let rootfs = guest.path("run/nvx/rootfs");
        fs::create_dir_all(rootfs.join("data/x")).unwrap();
        let export = guest.path("hostfs/root");
        fs::create_dir_all(export.join("0/src")).unwrap();
        guest.kernel.add_mount(&export);
        // A rename between the walk and the attach moves the target's directory, and the detach
        // of what went there fails.
        let renamed = rootfs.clone();
        guest.kernel.on_move(move |_| {
            fs::rename(renamed.join("data"), renamed.join("moved")).unwrap();
        });
        let moved = rootfs.join("moved/x");
        guest
            .kernel
            .fail(Operation::Detach, moved.clone(), Errno::EBUSY);
        let (error, mount) = if share {
            let error = root
                .mount_share("t", Path::new("/data/x"), Access::ReadWrite)
                .unwrap_err();
            let mount = Mount::Share {
                tag: "t".into(),
                target: "/data/x".into(),
            };
            (error, mount)
        } else {
            let source = SourceRoot::open(&export).unwrap();
            let mapping =
                crate::guest::mappings::Mapping::new("0/src", "/data/x", Access::ReadOnly).unwrap();
            let error = root.map(&source, &[mapping]).unwrap_err();
            let mount = Mount::Mapping {
                target: "/data/x".into(),
            };
            (error, mount)
        };
        assert!(
            matches!(
                &error,
                Error::Mapping {
                    error: MappingError::Moved { mounted: true },
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(root.is_latched());
        assert!(guest.kernel.mount_at(&moved).is_some());
        assert_eq!(guest.kernel.mount_at(&guest.path("run/nvx/staging")), None);
        // The teardown detaches it first, wherever it is, or reports it.
        if detach.is_ok() {
            guest.kernel.restore(Operation::Detach, &moved);
        }
        let calls = guest.kernel.calls().len();
        let torn = root.teardown();
        assert_eq!(
            guest.kernel.calls()[calls],
            Call::Detach(moved.display().to_string())
        );
        assert_eq!(guest.kernel.mount_at(&moved).is_some(), detach.is_err());
        match detach {
            Ok(()) => torn.unwrap(),
            Err(busy) => {
                let error = torn.unwrap_err();
                let failures: Vec<_> = error
                    .failures()
                    .iter()
                    .map(|failure| (failure.mount().clone(), failure.path().to_owned()))
                    .collect();
                assert_eq!(failures, [(mount, rootfs.join("data/x"))]);
                assert_eq!(
                    error.failures()[0].error().raw_os_error(),
                    Some(busy as i32)
                );
            }
        }
    }
}

#[test]
fn mappings_apply_beneath_the_overlay_in_order_and_go_first_at_the_teardown() {
    let guest = Guest::new();
    let mut root = guest.assemble(&guest.assembly()).unwrap();
    let export = guest.path("hostfs/root");
    fs::create_dir_all(export.join("0/src")).unwrap();
    fs::write(export.join("0/notes.txt"), b"notes").unwrap();
    guest.kernel.add_mount(&export);
    let source = SourceRoot::open(&export).unwrap();
    let calls = guest.kernel.calls().len();
    // Zero mappings do no work.
    root.map(&source, &[]).unwrap();
    assert_eq!(guest.kernel.calls().len(), calls);
    let mapping = |source: &str, target: &str, access| {
        crate::guest::mappings::Mapping::new(source, target, access).unwrap()
    };
    root.map(
        &source,
        &[
            mapping("0/src", "/workspace", Access::ReadWrite),
            mapping("0/notes.txt", "/home/user/notes", Access::ReadOnly),
        ],
    )
    .unwrap();
    let mut expected = attach_calls(
        &export.join("0/src"),
        &guest.path("run/nvx/rootfs/workspace"),
        Access::ReadWrite,
    );
    expected.extend(attach_calls(
        &export.join("0/notes.txt"),
        &guest.path("run/nvx/rootfs/home/user/notes"),
        Access::ReadOnly,
    ));
    assert_eq!(guest.kernel.calls()[calls..], expected);
    assert_eq!(targets(&root), ["/workspace", "/home/user/notes"]);
    // A share may not shadow a mapping, nor be inside one.
    let error = root
        .mount_share("t", Path::new("/workspace/inner"), Access::ReadWrite)
        .unwrap_err();
    assert!(
        matches!(&error, Error::Share(ShareRefusal::Overlap { claimed, .. })
            if *claimed == Path::new("/workspace")),
        "{error:?}"
    );
    // NVX's container root reserves its runtime's targets; the refusal latches the root.
    let error = root
        .map(&source, &[mapping("0/src", "/etc", Access::ReadOnly)])
        .unwrap_err();
    assert!(
        matches!(
            &error,
            Error::Mapping { mount: Mount::Mapping { target }, error: MappingError::Reserved }
                if *target == Path::new("/etc")
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "failed to mount the mapping at /etc: the runtime reserves the target"
    );
    let error = root
        .map(&source, &[mapping("0/src", "/srv", Access::ReadOnly)])
        .unwrap_err();
    assert!(
        matches!(
            &error,
            Error::Mapping {
                error: MappingError::Latched,
                ..
            }
        ),
        "{error:?}"
    );
    root.map(&source, &[]).unwrap();
    assert!(!guest.path("run/nvx/rootfs/srv").exists());
    root.teardown().unwrap();
    assert_eq!(
        guest.kernel.unmounts(),
        [
            guest.text("run/nvx/rootfs/home/user/notes"),
            guest.text("run/nvx/rootfs/workspace"),
            guest.text("run/nvx/rootfs"),
            guest.text("run/nvx/layers/distro"),
            guest.text("run/nvx/scratch"),
        ]
    );
}

#[test]
fn a_root_in_run_nvx_mounts_the_host_export_where_its_boot_token_names_it() {
    assert_eq!(
        Path::new("/run/nvx").join(EXPORT_DIRECTORY),
        Path::new(crate::protocol::export::EXPORT_DIRECTORY)
    );
    assert_eq!(
        Path::new(EXPORT_DIRECTORY).parent(),
        Some(Path::new(HOSTFS_DIRECTORY))
    );
}

#[test]
fn the_host_export_is_mounted_out_of_the_root_and_unmounted_after_its_mappings() {
    let restricted = MountFlags::MS_NOSUID | MountFlags::MS_NODEV;
    for (access, flags) in [
        (Access::ReadOnly, restricted | MountFlags::MS_RDONLY),
        (Access::ReadWrite, restricted),
    ] {
        let guest = Guest::new();
        let mut root = guest.assemble(&guest.assembly()).unwrap();
        let assembled = guest.kernel.calls().len();
        let source = root.mount_export("microvm", access).unwrap();
        // The export's device alone is mounted, restricted, out of the root, below a directory
        // that only root may enter.
        let mount_point = guest.path("run/nvx/hostfs/root");
        assert_eq!(
            guest.kernel.calls()[assembled..],
            [mount_call(
                Some("microvm"),
                &guest.text("run/nvx/hostfs/root"),
                Some("virtiofs"),
                flags,
                None,
            )]
        );
        assert!(mount_point.is_dir());
        let mode = fs::metadata(guest.path("run/nvx/hostfs")).unwrap().mode() & 0o7777;
        assert_eq!(mode, 0o700, "{access:?}");
        assert_eq!(
            fs::read_dir(guest.path("run/nvx/rootfs")).unwrap().count(),
            0
        );
        assert!(!root.is_latched());

        // Its root is the source of the caller's mappings: a directory child, `0`, and a file
        // child, `1`, of the aggregate.
        fs::create_dir_all(mount_point.join("0/src")).unwrap();
        fs::write(mount_point.join("1"), b"notes").unwrap();
        let mapped = guest.kernel.calls().len();
        root.map(
            &source,
            &[
                Mapping::new("0/src", "/workspace", Access::ReadWrite).unwrap(),
                Mapping::new("1", "/home/user/notes", Access::ReadOnly).unwrap(),
            ],
        )
        .unwrap();
        let mut expected = attach_calls(
            &mount_point.join("0/src"),
            &guest.path("run/nvx/rootfs/workspace"),
            Access::ReadWrite,
        );
        expected.extend(attach_calls(
            &mount_point.join("1"),
            &guest.path("run/nvx/rootfs/home/user/notes"),
            Access::ReadOnly,
        ));
        assert_eq!(guest.kernel.calls()[mapped..], expected);
        assert_eq!(targets(&root), ["/workspace", "/home/user/notes"]);

        // A root mounts one export, since every export is mounted at the same place; refusing
        // another latches nothing.
        let mounts = guest.kernel.mounts().len();
        let error = root.mount_export("other", access).unwrap_err();
        assert!(
            matches!(error, Error::Share(ShareRefusal::SecondExport)),
            "{error:?}"
        );
        assert_eq!(error.to_string(), "the root has a host export already");
        assert_eq!(guest.kernel.mounts().len(), mounts);
        assert!(!root.is_latched());

        // An aggregate live share, mounted later, is unmounted before the export.
        export(&guest, &[("a", ShareKind::Directory)]);
        let rw = Access::ReadWrite;
        root.mount_aggregate(
            "all",
            rw,
            &[child("a", "/data/a", rw, ShareKind::Directory)],
        )
        .unwrap();

        // The teardown unmounts the live shares and mappings, those from the export included,
        // then the aggregate and the export, the later first, then the assembly's mounts.
        drop(source);
        root.teardown().unwrap();
        assert_eq!(
            guest.kernel.unmounts(),
            [
                guest.text("run/nvx/rootfs/data/a"),
                guest.text("run/nvx/rootfs/home/user/notes"),
                guest.text("run/nvx/rootfs/workspace"),
                guest.text("run/nvx/shares"),
                guest.text("run/nvx/hostfs/root"),
                guest.text("run/nvx/rootfs"),
                guest.text("run/nvx/layers/distro"),
                guest.text("run/nvx/scratch"),
            ]
        );
    }
}

#[test]
fn a_refused_tag_creates_and_mounts_nothing_and_a_failed_export_latches_the_root() {
    let guest = Guest::new();
    let mut root = guest.assemble(&guest.assembly()).unwrap();
    let calls = guest.kernel.calls();
    for tag in ["", "a b", "a.b", "a/b", "a,b", "a=b", "micro\u{e9}vm"] {
        let error = root.mount_export(tag, Access::ReadOnly).unwrap_err();
        assert!(
            matches!(&error, Error::Share(ShareRefusal::Tag(refused)) if refused == tag),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!("live share tag is invalid: {tag}")
        );
    }
    assert_eq!(guest.kernel.calls(), calls);
    assert!(!guest.path("run/nvx/hostfs").exists());
    // The refusals of requests latch nothing.
    assert!(!root.is_latched());

    // A device that fails to mount latches the root, and leaves nothing to unmount.
    let mount_point = guest.path("run/nvx/hostfs/root");
    guest
        .kernel
        .fail(Operation::Mount, mount_point.clone(), Errno::ENODEV);
    let error = root.mount_export("microvm", Access::ReadOnly).unwrap_err();
    assert!(
        matches!(
            &error,
            Error::Mount { mount: Mount::Export { tag }, error }
                if tag == "microvm" && error.raw_os_error() == Some(Errno::ENODEV as i32)
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "failed to mount the host export microvm: {}",
            os_error(Errno::ENODEV)
        )
    );
    assert!(root.is_latched());

    // A latched root refuses the export, as it refuses every share and mapping.
    guest.kernel.restore(Operation::Mount, &mount_point);
    let mounts = guest.kernel.mounts().len();
    let error = root.mount_export("microvm", Access::ReadOnly).unwrap_err();
    assert!(
        matches!(
            &error,
            Error::Mapping { mount: Mount::Export { tag }, error: MappingError::Latched }
                if tag == "microvm"
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "failed to mount the host export microvm: an earlier mapping failed"
    );
    assert_eq!(guest.kernel.mounts().len(), mounts);
    root.teardown().unwrap();
    assert_eq!(
        guest.kernel.unmounts(),
        [
            guest.text("run/nvx/rootfs"),
            guest.text("run/nvx/layers/distro"),
            guest.text("run/nvx/scratch"),
        ]
    );
}

#[test]
fn the_host_export_is_mounted_through_no_symbolic_link() {
    for link in ["run/nvx/hostfs", "run/nvx/hostfs/root"] {
        let guest = Guest::new();
        let mut root = guest.assemble(&guest.assembly()).unwrap();
        fs::create_dir(guest.path("elsewhere")).unwrap();
        let parent = guest.path(link).parent().unwrap().to_owned();
        if !parent.exists() {
            // A private `hostfs`, as an earlier root leaves it.
            fs::create_dir(&parent).unwrap();
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        }
        std::os::unix::fs::symlink(guest.path("elsewhere"), guest.path(link)).unwrap();
        let mounts = guest.kernel.mounts().len();
        let error = root.mount_export("microvm", Access::ReadOnly).unwrap_err();
        assert!(
            matches!(
                &error,
                Error::MountPoint { path, error }
                    if *path == guest.path(link)
                        && error.raw_os_error() == Some(Errno::ENOTDIR as i32)
            ),
            "{link}: {error:?}"
        );
        assert_eq!(guest.kernel.mounts().len(), mounts, "{link}");
        assert_eq!(
            fs::read_dir(guest.path("elsewhere")).unwrap().count(),
            0,
            "{link}"
        );
        assert!(root.is_latched(), "{link}");
        root.teardown().unwrap();
    }
}

#[test]
fn a_hostfs_directory_that_is_there_already_must_be_private() {
    // One that its group or others may enter is refused, keeps its mode, and latches the root.
    let user = nix::unistd::geteuid().as_raw();
    for mode in [0o750, 0o705, 0o711] {
        let guest = Guest::new();
        let mut root = guest.assemble(&guest.assembly()).unwrap();
        let hostfs = guest.path("run/nvx/hostfs");
        fs::create_dir(&hostfs).unwrap();
        fs::set_permissions(&hostfs, fs::Permissions::from_mode(mode)).unwrap();
        let mounts = guest.kernel.mounts().len();
        let error = root.mount_export("microvm", Access::ReadOnly).unwrap_err();
        assert!(
            matches!(
                &error,
                Error::MountPoint { path, error }
                    if *path == hostfs && error.kind() == io::ErrorKind::PermissionDenied
            ),
            "{mode:o}: {error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!(
                "failed to prepare {}: the directory there already is not private to user \
                 {user}: user {user} owns it with mode {mode:04o}",
                hostfs.display()
            )
        );
        assert_eq!(guest.kernel.mounts().len(), mounts, "{mode:o}");
        assert!(!guest.path("run/nvx/hostfs/root").exists(), "{mode:o}");
        assert_eq!(fs::metadata(&hostfs).unwrap().mode() & 0o7777, mode);
        assert!(root.is_latched(), "{mode:o}");
        root.teardown().unwrap();
    }

    // One that only its owner, the process's effective user, may enter serves, such as the one
    // that an earlier root left in the runtime directory.
    let guest = Guest::new();
    let mut root = guest.assemble(&guest.assembly()).unwrap();
    drop(root.mount_export("microvm", Access::ReadOnly).unwrap());
    root.teardown().unwrap();
    let mut root = guest.assemble(&guest.assembly()).unwrap();
    drop(root.mount_export("microvm", Access::ReadWrite).unwrap());
    let mode = fs::metadata(guest.path("run/nvx/hostfs")).unwrap().mode() & 0o7777;
    assert_eq!(mode, 0o700);
    assert!(!root.is_latched());
    root.teardown().unwrap();
    let unmounts = [
        guest.text("run/nvx/hostfs/root"),
        guest.text("run/nvx/rootfs"),
        guest.text("run/nvx/layers/distro"),
        guest.text("run/nvx/scratch"),
    ];
    assert_eq!(
        guest.kernel.unmounts(),
        [unmounts.clone(), unmounts].concat()
    );
}

#[test]
fn a_dropped_root_is_torn_down_once() {
    let guest = Guest::new();
    let root = guest.assemble(&guest.assembly()).unwrap();
    guest.kernel.fail(
        Operation::Unmount,
        guest.path("run/nvx/rootfs"),
        Errno::EBUSY,
    );
    drop(root);
    let unmounts = [
        guest.text("run/nvx/rootfs"),
        guest.text("run/nvx/layers/distro"),
        guest.text("run/nvx/scratch"),
    ];
    assert_eq!(guest.kernel.unmounts(), unmounts);

    // A root that was torn down has nothing left to unmount when it is dropped.
    let guest = Guest::new();
    guest
        .assemble(&guest.assembly())
        .unwrap()
        .teardown()
        .unwrap();
    assert_eq!(guest.kernel.unmounts().len(), 3);
}

/// A process of the bootstrap's parent cgroup.
enum Process {
    /// A process, which the bootstrap moves.
    User(u32),
    /// A kernel thread, which stays.
    Kernel(u32),
    /// A process that ended after the cgroup listed it, which has no `/proc` entry.
    Ended(u32),
}

/// Prepares a cgroup hierarchy at `cgroup` in `guest`, whose root, already a cgroup if
/// `mounted`, holds `processes`, and returns the layout of its `agent` and `container` cgroups.
fn hierarchy(guest: &Guest, mounted: bool, processes: &[Process]) -> Layout {
    let cgroup = guest.path("cgroup");
    write(
        &guest.paths.proc.join("sys/user/max_user_namespaces"),
        "15019\n",
    );
    if mounted {
        write(&cgroup.join("cgroup.controllers"), "cpu io memory pids\n");
    }
    write(&cgroup.join("cgroup.subtree_control"), "");
    let mut members = String::new();
    for process in processes {
        let (id, flags) = match *process {
            Process::User(id) => (id, Some(0x0040_0100)),
            Process::Kernel(id) => (id, Some(0x0020_8040)),
            Process::Ended(id) => (id, None),
        };
        members.push_str(&format!("{id}\n"));
        if let Some(flags) = flags {
            let stat = format!("{id} (p {id}) S 0 {id} {id} 0 -1 {flags} 0 0 0 0 0 0 0 0 20 0 1\n");
            write(&guest.paths.proc.join(id.to_string()).join("stat"), stat);
        }
    }
    write(&cgroup.join("cgroup.procs"), members);
    Layout::new(cgroup.join("agent"), cgroup.join("container")).unwrap()
}

/// Bootstraps `layout` with `resources` and a clone of `guest`'s kernel.
fn bootstrap_in(guest: &Guest, layout: &Layout, resources: &Resources) -> Result<Bootstrap, Error> {
    bootstrap::bootstrap_with(&mut guest.kernel.clone(), &guest.paths, layout, resources)
}

/// Creates the interface files `files` of the workload cgroup, `cgroup/container`, in `guest`,
/// as the kernel shows them once its parent enables their controllers, which a hierarchy of
/// regular files does not.
fn interface_files(guest: &Guest, files: &[&str]) {
    for file in files {
        write(&guest.path("cgroup/container").join(file), "");
    }
}

#[test]
fn bootstrap_moves_every_process_but_kernel_threads_and_applies_the_resources() {
    let guest = Guest::bare();
    let processes = [
        Process::User(1),
        Process::Kernel(2),
        Process::User(45),
        Process::Ended(99),
    ];
    let layout = hierarchy(&guest, true, &processes);
    interface_files(&guest, &["pids.current", "memory.max", "pids.max"]);
    let resources = Resources {
        agent_memory_low: Some(16 << 20),
        workload_memory_max: Some(512 << 20),
        workload_pids_max: Some(65),
    };
    let bootstrap = bootstrap_in(&guest, &layout, &resources).unwrap();
    assert_eq!(bootstrap.moved(), [1, 45]);
    assert_eq!(bootstrap.layout(), &layout);
    assert_eq!(bootstrap.workload().path(), layout.workload());
    let writes: Vec<(String, String)> = [
        ("proc/sys/user/max_user_namespaces", "0"),
        ("cgroup/agent/cgroup.procs", "1"),
        ("cgroup/agent/cgroup.procs", "45"),
        ("cgroup/agent/memory.low", "16777216"),
    ]
    .into_iter()
    .map(|(path, value)| (guest.text(path), value.to_owned()))
    .collect();
    assert_eq!(guest.kernel.writes(), writes);
    assert!(guest.kernel.mounts().is_empty());
    // The agent's `memory.low` enabled the `memory` controller. The workload cgroup has every
    // file already, so its preparation enabled nothing, and wrote its values.
    assert_eq!(
        fs::read_to_string(guest.path("cgroup/cgroup.subtree_control")).unwrap(),
        "+memory"
    );
    for (file, value) in [("memory.max", "536870912"), ("pids.max", "65")] {
        assert_eq!(
            fs::read_to_string(guest.path("cgroup/container").join(file)).unwrap(),
            value,
            "{file}"
        );
    }
    assert_eq!(
        fs::read_to_string(guest.path("cgroup/cgroup.procs")).unwrap(),
        "2\n99\n"
    );
    assert_eq!(
        fs::read_to_string(guest.path("cgroup/agent/cgroup.procs")).unwrap(),
        "1\n45\n"
    );
}

#[test]
fn bootstrap_mounts_the_hierarchy_unless_the_parent_is_a_cgroup() {
    let guest = Guest::bare();
    let layout = hierarchy(&guest, false, &[Process::User(1)]);
    bootstrap_in(&guest, &layout, &Resources::default()).unwrap();
    assert_eq!(
        guest.kernel.calls()[1],
        mount_call(
            Some("cgroup2"),
            &guest.text("cgroup"),
            Some("cgroup2"),
            MountFlags::MS_NOSUID | MountFlags::MS_NODEV | MountFlags::MS_NOEXEC,
            None,
        )
    );
    // Without a value, the bootstrap writes only the placement, and the preparation of the new
    // workload cgroup enables only the `pids` controller.
    assert_eq!(guest.kernel.writes().len(), 2);
    assert_eq!(
        fs::read_to_string(guest.path("cgroup/cgroup.subtree_control")).unwrap(),
        "+pids"
    );
    assert!(guest.path("cgroup/container").is_dir());
}

#[test]
fn bootstrap_names_the_failed_step_and_keeps_the_earlier_changes() {
    // The workload cgroup's `memory.max` cannot be written: a directory takes its place.
    let guest = Guest::bare();
    let layout = hierarchy(&guest, true, &[Process::User(1)]);
    interface_files(&guest, &["pids.current", "pids.max"]);
    fs::create_dir(guest.path("cgroup/container/memory.max")).unwrap();
    let resources = Resources {
        agent_memory_low: Some(1 << 20),
        workload_memory_max: Some(1),
        workload_pids_max: Some(8),
    };
    let error = bootstrap_in(&guest, &layout, &resources).unwrap_err();
    assert!(
        matches!(
            &error,
            Error::Bootstrap { step: BootstrapStep::WorkloadCgroup, error }
                if error.raw_os_error() == Some(Errno::EISDIR as i32)
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "failed to prepare the workload cgroup: {}",
            os_error(Errno::EISDIR)
        )
    );
    assert_eq!(
        fs::read_to_string(guest.path("cgroup/agent/memory.low")).unwrap(),
        "1048576"
    );

    // The agent's value is refused before the workload cgroup is prepared.
    let guest = Guest::bare();
    let layout = hierarchy(&guest, true, &[Process::User(1)]);
    guest.kernel.fail(
        Operation::Write,
        guest.path("cgroup/agent/memory.low"),
        Errno::EINVAL,
    );
    let error = bootstrap_in(&guest, &layout, &resources).unwrap_err();
    assert!(
        matches!(
            &error,
            Error::Bootstrap { step: BootstrapStep::AgentMemoryLow, error }
                if error.raw_os_error() == Some(Errno::EINVAL as i32)
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "failed to set the agent's memory reservation: {}",
            os_error(Errno::EINVAL)
        )
    );
    assert_eq!(
        fs::read_to_string(guest.path("cgroup/agent/cgroup.procs")).unwrap(),
        "1\n"
    );
    assert!(!guest.path("cgroup/container").exists());

    // Cgroups that are not siblings are refused before anything changes.
    let guest = Guest::bare();
    hierarchy(&guest, true, &[Process::User(1)]);
    let cgroup = guest.path("cgroup");
    let layout = Layout::new(cgroup.join("agent"), cgroup.join("workloads/container")).unwrap();
    let error = bootstrap_in(&guest, &layout, &Resources::default()).unwrap_err();
    assert!(
        matches!(
            &error,
            Error::Bootstrap { step: BootstrapStep::Layout, error }
                if error.raw_os_error() == Some(Errno::EINVAL as i32)
        ),
        "{error:?}"
    );
    assert!(guest.kernel.calls().is_empty());
}

#[test]
fn bootstrap_accepts_a_missing_sysctl_only_without_user_namespaces() {
    // A kernel without sysctls lacks the limit, which one without user namespaces needs no more.
    let guest = Guest::bare();
    let layout = hierarchy(&guest, true, &[Process::User(1)]);
    guest.kernel.fail(
        Operation::Write,
        guest.path("proc/sys/user/max_user_namespaces"),
        Errno::ENOENT,
    );
    bootstrap_in(&guest, &layout, &Resources::default()).unwrap();
    assert_eq!(
        fs::read_to_string(guest.path("cgroup/agent/cgroup.procs")).unwrap(),
        "1\n"
    );

    // One with user namespaces cannot be limited, so nothing else changes.
    let guest = Guest::bare();
    let layout = hierarchy(&guest, true, &[Process::User(1)]);
    guest.kernel.fail(
        Operation::Write,
        guest.path("proc/sys/user/max_user_namespaces"),
        Errno::ENOENT,
    );
    write(&guest.paths.proc.join("self/ns/user"), "");
    let error = bootstrap_in(&guest, &layout, &Resources::default()).unwrap_err();
    assert!(
        matches!(
            &error,
            Error::Bootstrap { step: BootstrapStep::UserNamespaces, error }
                if error.raw_os_error() == Some(Errno::ENOENT as i32)
        ),
        "{error:?}"
    );
    assert_eq!(guest.kernel.calls().len(), 1);
    assert!(!guest.path("cgroup/agent").exists());
}

#[test]
fn bootstrap_fails_while_a_process_keeps_returning_to_the_parent() {
    let guest = Guest::bare();
    let layout = hierarchy(&guest, true, &[Process::User(1), Process::User(7)]);
    guest.kernel.pin(7, usize::MAX);
    let error = bootstrap_in(&guest, &layout, &Resources::default()).unwrap_err();
    assert!(
        matches!(
            &error,
            Error::Bootstrap { step: BootstrapStep::Placement, error }
                if error.raw_os_error() == Some(Errno::EBUSY as i32)
        ),
        "{error:?}"
    );
    let attempts = guest
        .kernel
        .writes()
        .into_iter()
        .filter(|(path, value)| path.ends_with("agent/cgroup.procs") && value == "7")
        .count();
    assert_eq!(attempts, 16);
}

#[test]
fn bootstrap_succeeds_when_the_last_pass_empties_the_parent() {
    let guest = Guest::bare();
    let layout = hierarchy(&guest, true, &[Process::User(1), Process::User(7)]);
    // The sixteenth pass is the first that moves the process.
    guest.kernel.pin(7, 15);
    let bootstrap = bootstrap_in(&guest, &layout, &Resources::default()).unwrap();
    assert_eq!(bootstrap.moved()[0], 1);
    assert_eq!(bootstrap.moved()[1..], [7; 16]);
    assert_eq!(
        fs::read_to_string(guest.path("cgroup/cgroup.procs")).unwrap(),
        ""
    );
    assert_eq!(
        fs::read_to_string(guest.path("cgroup/agent/cgroup.procs")).unwrap(),
        "1\n7\n"
    );
}
