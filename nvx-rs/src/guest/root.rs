//! Operations that bootstrap a guest for a layered root, assemble the root, mount live shares and
//! mappings in it, and the host export from which mappings come, and tear it down, for a guest
//! init or agent, whether or not it is PID 1.
//!
//! - [`bootstrap`] prepares the guest once: it creates the agent's and the workload's sibling
//!   cgroups of a [`Layout`](super::cgroup::Layout), moves every process of their parent into the
//!   agent's, applies the agent's and the workload's resource values, and keeps user namespaces
//!   unavailable.
//! - [`assemble`] finds the block devices of an [`Assembly`] by their MMIO windows, checks and
//!   flushes them, and mounts the root: an overlay, mounted `nosuid,nodev` with the option set of
//!   the caller's [`OverlayMode`], which it checks the kernel applied, of the read-only layers of
//!   the `distro` image over an upper. The caller names the image's format, which the assembly
//!   never probes: an EROFS file system, the overlay's one lower layer, or a GPT disk, whose first
//!   partition holds opaque bytes that the root returns, and whose later partitions hold the
//!   layers, ext4 file systems. The upper is an ext4 scratch or a tmpfs. It returns the [`Root`]
//!   that owns the mounts, holds the [runtime directory](#runtime-directory), and reports what it
//!   mounted: the first partition's bytes, the [`Lower`] layers, the [`UpperKind`], and the
//!   [`OverlayMode`].
//! - [`Root::mount_share`] and [`Root::mount_aggregate`] mount the VMM's live shares at targets in
//!   the root, [`Root::mount_export`] mounts the host export out of the root, and [`Root::map`]
//!   applies a caller's host mappings in the root, from the export or another source, through the
//!   root's [`Mapper`], as [live shares and mappings](#live-shares-and-mappings) describes.
//! - [`Root::teardown`] unmounts, in order, the live shares and mappings in reverse order, the
//!   mounts of an aggregate live share and of the host export, the later first, the overlay, the
//!   layers from the top down, and the upper, whose file system it syncs first if the upper is an
//!   ext4 scratch. It attempts each, whatever failed before, and reports every sync or unmount
//!   that failed. A root that is dropped tears down the same way, but ignores failures.
//!
//! A failure is an [`Error`] that names what failed. The operations need the guest's root user,
//! in the guest's initial mount and cgroup namespaces. They never collect a child's status,
//! change the caller's root directory or credentials, start an async runtime, or power the VM
//! off, so a caller that is not PID 1 keeps its own reaping, transport, and shutdown.
//!
//! # Runtime directory
//!
//! The root's mounts are below a runtime directory that the caller chooses, such as `/run/nvx`,
//! which holds:
//!
//! - `layers/distro`, an EROFS image, or `layers/pN`, the layer in partition N of a GPT disk, for
//!   each partition after the first;
//! - `scratch`, an ext4 scratch, or `tmpfs`, a tmpfs, with the overlay's `upper` and `work`
//!   directories;
//! - `rootfs`, the overlay, which is the root;
//! - `shares`, an aggregate live share, whose root lists every child, out of the workload's
//!   reach;
//! - `hostfs/root`, the host export, whose root lists every exported host directory and file, out
//!   of the workload's reach, in `hostfs`, which only root may enter;
//! - and `staging`, where a live share is mounted, out of the workload's reach, before it moves
//!   to its target.
//!
//! A root mounts and unmounts by these paths, so they must lead to the same directories whatever
//! the root has mounted. An assembly refuses a runtime directory whose path is not canonical, as
//! a live share's target must be, or leads through a symbolic link, and a root mounts nothing
//! where `layers`, `hostfs`, or the mount point is a symbolic link, since a `..` component or a
//! link could lead through a directory that one of the root's mounts hides later. A root also
//! holds the exclusive lock of its runtime directory, from the start of its assembly until it is
//! torn down: an assembly refuses a runtime directory that another root holds, in this process or
//! another, and one where a file system, or a bind mount of a directory, is mounted already at any
//! of these mount points but `staging`, whether or not the new root mounts there, such as an upper
//! of either kind, or a layer of either image, that a failed teardown left; a live share refuses a
//! `staging` where one is mounted already. The lock is advisory: it keeps out the roots of these
//! operations, not other mounts.
//!
//! The overlay's options name its layers' and its upper's directories by these paths, and
//! `mount(2)` passes a file system at most [`MOUNT_OPTIONS_LIMIT`] bytes of options. An assembly
//! refuses longer options before it mounts anything, rather than let the kernel cut them short,
//! so the length of the runtime directory's path limits the layers of a GPT disk: below
//! `/run/nvx`, a root mounts up to 195 layers, those of partitions 2 through 196, whichever the
//! overlay mode and the upper.
//!
//! # Live shares and mappings
//!
//! A live share is a virtio-fs device that the VMM exports from the host: one host directory,
//! mounted at a target in the root, or an aggregate of host directories and files, each of which,
//! a child, is bound at its own target. A root has one aggregate at most. Before it mounts
//! anything for a share, the root checks that each target:
//!
//! - is absolute and canonical: not `/`, without an empty, `.`, or `..` component, a trailing
//!   `/`, or a NUL, and of at most [`MAX_PATH_BYTES`](super::mappings::MAX_PATH_BYTES) bytes, as
//!   the target of a [`Mapping`](super::mappings::Mapping) is;
//! - holds no `\`, `=`, `,`, or `"`;
//! - is not where a container runtime mounts or binds in the root, as
//!   [`protocol::share`](crate::protocol::share) lists them for the host and the guest: `/proc`,
//!   `/sys`, `/dev`, `/.nvx-agent`, `/etc/machine-id`, `/etc/resolv.conf`, `/run/nvx/time`, or
//!   below them, nor `/etc`, `/run`, or `/run/nvx`, below which it mounts;
//! - and is not another share's or mapping's target, nor inside one, nor holds one.
//!
//! The root's [`Mapper`] then attaches the shares beneath the overlay's descriptor, as
//! [`mappings`](super::mappings) describes, with NVX's container root's reserved targets,
//! [`Reserved::container_root`](super::mappings::Reserved::container_root): it walks to each
//! target through no symbolic link or magic link; creates each missing component only on the
//! overlay, a directory with mode 0755, or a file child's file with mode 0644, whatever the
//! umask or a default ACL; checks that the target is a directory, or a regular file for a child
//! that is a host file; and attaches each mount with its [`Access`](super::mappings::Access),
//! `nosuid`, and `nodev` set already. The root mounts a share's device at `staging`, with them,
//! then moves it onto its target, and mounts an aggregate's device at `shares`, then attaches a
//! copy of each child's entry there.
//! [`Root::map`] applies a caller's host [`Mapping`](super::mappings::Mapping)s beneath the
//! overlay the same way, from the caller's [`SourceRoot`](super::mappings::SourceRoot).
//!
//! The host export is the device of an aggregate whose children the root never binds itself:
//! they are the sources of the caller's mappings, such as the mapping table of NVX's managed
//! lifecycle, whose boot tokens [`protocol::export`](crate::protocol::export) reads.
//! [`Root::mount_export`] mounts it at `hostfs/root`, out of the root, with the access of its
//! [`ExportMode`](crate::protocol::export::ExportMode), `nosuid`, and `nodev`, and returns its
//! root, the [`SourceRoot`](super::mappings::SourceRoot) from which [`Root::map`] applies the
//! caller's mappings. A root has one export at most.
//!
//! The mounts are made in the caller's mount namespace, which must be recursively private, as a
//! guest's initial namespace is. Mount-point checks and mount-option reads use the calling
//! thread's descriptor table and mount namespace, even when it shares neither with the process's
//! first thread. A share, a child, the export, or a mapping that fails once the root has checked
//! its request latches the root: the root refuses every later share, export, and
//! mapping, so that its caller starts no workload on a partial set, and the mounts before it stay
//! until the teardown, which unmounts them first.
//!
//! ```no_run
//! use std::path::Path;
//! use std::time::Duration;
//!
//! use nvx::gpt::Expected;
//! use nvx::guest::cgroup::Layout;
//! use nvx::guest::mappings::{Access, Mapping};
//! use nvx::guest::root::{self, Assembly, Device, Image, OverlayMode, Resources, Upper};
//! use nvx::protocol::export::HostExport;
//! use nvx::protocol::root::DISTRO_MMIO_BASE;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let layout = Layout::new("/sys/fs/cgroup/agent", "/sys/fs/cgroup/container")?;
//! let resources = Resources {
//!     agent_memory_low: Some(16 << 20),
//!     ..Resources::default()
//! };
//! let bootstrap = root::bootstrap(&layout, &resources)?;
//! let mut root = root::assemble(&Assembly {
//!     runtime: "/run/nvx".into(),
//!     distro: Device { mmio: DISTRO_MMIO_BASE, sectors: None },
//!     image: Image::Gpt {
//!         expected: Expected::new().disk_guid("8d2f3a4e-1c5b-4e7a-9f60-2b3c4d5e6f70".parse()?),
//!         first_partition_bound: 64 * 1024,
//!     },
//!     upper: Upper::Tmpfs,
//!     overlay: OverlayMode::UntrustedLowers,
//!     timeout: Duration::from_secs(30),
//! })?;
//! let configuration = root.first_partition().unwrap_or_default();
//! let layers = root.lowers().len();
//! println!("the first partition holds {} bytes over {layers} layers", configuration.len());
//! root.mount_share("workspace", Path::new("/workspace"), Access::ReadWrite)?;
//! // The host export that the kernel command line names, if any, is the source of host mappings.
//! if let Some(export) = HostExport::parse(&std::fs::read("/proc/cmdline")?)? {
//!     let source = root.mount_export(&export.tag, export.mode.into())?;
//!     root.map(&source, &[Mapping::new("0/src", "/src", Access::ReadOnly)?])?;
//!     // Dropping the export's root here lets the teardown unmount the export.
//! }
//! // Run the workload in `bootstrap.workload()`, in the root at `root.path()`; then:
//! root.teardown()?;
//! # Ok(())
//! # }
//! ```

mod block;
mod bootstrap;
mod kernel;
mod share;
#[cfg(test)]
mod tests;

use std::fmt;
use std::fs::{self, File};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use self::kernel::{Kernel, SystemKernel};
use super::gpt::DeviceError;
use super::mappings::{Mapper, MappingError, below_its_root};
use super::sys::{self, Errno, MountFlags};
use crate::gpt::{Expected, GptError, Partition};
use crate::uuid::Uuid;

pub(crate) use self::block::window_directories;
pub use self::bootstrap::{Bootstrap, BootstrapStep, Resources, bootstrap};
pub(crate) use self::kernel::Paths;
#[cfg(test)]
pub(crate) use self::kernel::{Call, FakeKernel, Operation, attach_calls};
pub use self::share::{ShareChild, ShareKind, ShareRefusal};

/// The directory of the layers' mount points, below the runtime directory.
const LAYERS_DIRECTORY: &str = "layers";
/// The EROFS image's mount point, below the runtime directory.
const DISTRO_DIRECTORY: &str = "layers/distro";
/// The ext4 scratch's mount point, below the runtime directory.
const SCRATCH_DIRECTORY: &str = "scratch";
/// The tmpfs upper's mount point, below the runtime directory.
const TMPFS_DIRECTORY: &str = "tmpfs";
/// The overlay's mount point, the root, below the runtime directory.
const ROOT_DIRECTORY: &str = "rootfs";
/// The mount point of an aggregate live share, below the runtime directory and outside the root,
/// which exists once a root has mounted an aggregate.
const AGGREGATE_DIRECTORY: &str = "shares";
/// The directory of the host export's mount point, below the runtime directory and outside the
/// root, which only root may enter, and which exists once a root has mounted an export.
const HOSTFS_DIRECTORY: &str = "hostfs";
/// The mount point of the host export, below the runtime directory: `root` in
/// [`HOSTFS_DIRECTORY`], which exists once a root has mounted an export.
const EXPORT_DIRECTORY: &str = "hostfs/root";
/// The mode of the [`HOSTFS_DIRECTORY`] that a root creates, whatever the process's umask, as
/// NVX's shell init creates it under `umask 077`: only root may enter it.
const HOSTFS_DIRECTORY_MODE: u32 = 0o700;
/// The mount point of a live share before it moves to its target, below the runtime directory
/// and outside the root, which exists once a root has mounted a share.
const STAGING_DIRECTORY: &str = "staging";
/// The overlay's upper directory, below the upper's mount point.
const UPPER_DIRECTORY: &str = "upper";
/// The overlay's work directory, below the upper's mount point.
const WORK_DIRECTORY: &str = "work";
/// The mode of the upper's `upper` and `work` directories that an assembly creates, whatever
/// the process's umask, as the shell init's `mkdir -p` created them under PID 1's: the overlay's
/// root directory takes the upper's mode, and a workload that is not root must enter its root.
const SCRATCH_DIRECTORY_MODE: u32 = 0o755;
/// The tmpfs upper's options: the mode of its root directory, which an ext4 scratch's root has
/// too, rather than tmpfs's world-writable default. Its size is the kernel's default.
const TMPFS_OPTIONS: &[u8] = b"mode=0755";
/// Do not load a GPT layer's journal: even without `needs_recovery`, loading can scan a corrupt
/// journal indefinitely. `check_layer` still refuses recovery-required layers. Override the
/// superblock's error policy too, so an ext4 error does not request a guest panic.
const LAYER_OPTIONS: &[u8] = b"noload,errors=remount-ro";
/// The characters that a runtime directory may not hold: the separators and escape of the
/// overlay's options, and the C string's terminator.
const RUNTIME_RESERVED: &[u8] = b",:\\=\0";
/// The directory of the overlay module's parameters, below sysfs, which hold the defaults of the
/// overlay's options.
const OVERLAY_PARAMETERS: &str = "module/overlay/parameters";

/// The most bytes of options that `mount(2)` passes a file system whole: one 4 KiB page, the
/// smallest into which Linux copies them, less the NUL that ends them. Linux cuts longer options
/// short, so [`assemble`] refuses an overlay whose options are longer.
pub const MOUNT_OPTIONS_LIMIT: usize = 4095;

/// The role of a block device in a layered root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Role {
    /// The read-only image: an EROFS file system, the overlay's lower layer, or a GPT disk, whose
    /// partitions after the first hold the overlay's lower layers.
    Distro,
    /// The writable ext4 scratch, which holds the overlay's upper and work directories.
    Scratch,
}

impl Role {
    /// Returns whether the role's device is read-only: the distro's is, and the scratch's is
    /// writable.
    const fn read_only(self) -> bool {
        matches!(self, Self::Distro)
    }
}

impl fmt::Display for Role {
    /// Writes `distro` or `scratch`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Distro => "distro",
            Self::Scratch => "scratch",
        })
    }
}

/// A block device that the VMM attaches for a role, which the base address of its MMIO window
/// names, as the VMM declares it on the kernel command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Device {
    /// The base address of the device's MMIO window.
    pub mmio: u64,
    /// The size that the device must have, in 512-byte sectors, if the caller expects one.
    pub sectors: Option<u64>,
}

/// A step on a found block device, which an [`Error::Device`] names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DeviceStep {
    /// Reading its size.
    Size,
    /// Flushing its buffers.
    Flush,
    /// Reading its EROFS or ext4 superblock.
    Superblock,
    /// Reading its access mode: read-only or writable.
    Access,
    /// Opening it, to read a GPT disk.
    Open,
    /// Reading the status of the node of a GPT disk's partition.
    Status,
    /// Rereading the partition table of the GPT disk on it, which a clone asks of the kernel before
    /// it assembles its root.
    Reread,
}

impl fmt::Display for DeviceStep {
    /// Writes the step as the verb phrase of a failure: `failed to {step} the device`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Size => "read the size of",
            Self::Flush => "flush the buffers of",
            Self::Superblock => "read the superblock of",
            Self::Access => "read the access mode of",
            Self::Open => "open",
            Self::Status => "read the status of",
            Self::Reread => "reread the partition table of",
        })
    }
}

/// What [`assemble`] mounts, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assembly {
    /// The runtime directory: an absolute path in canonical form, not `/`, without an empty, `.`,
    /// or `..` component, or a trailing `/`, of at most
    /// [`MAX_PATH_BYTES`](super::mappings::MAX_PATH_BYTES) bytes, that no symbolic link leads
    /// through, and without `,`, `:`, `\`, `=`, or NUL, which the overlay's options reserve.
    pub runtime: PathBuf,
    /// The distro device, which holds the image.
    pub distro: Device,
    /// The image's format, and what the caller expects of it.
    pub image: Image,
    /// The upper, which holds the overlay's upper and work directories.
    pub upper: Upper,
    /// The overlay's mode, which selects its option set.
    pub overlay: OverlayMode,
    /// How long to wait for each device to appear.
    pub timeout: Duration,
}

/// The image on the distro device, in the format that the caller names: an assembly never probes
/// the device for another.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Image {
    /// An EROFS file system: the overlay's one lower layer, which an assembly mounts read-only at
    /// `layers/distro`.
    Erofs {
        /// The UUID that the file system's superblock must hold, if the caller expects one.
        uuid: Option<Uuid>,
    },
    /// A GPT disk, whose partition table an assembly reads and checks through
    /// [`guest::gpt::read_disk`](super::gpt::read_disk), and whose partitions it uses as the
    /// kernel created them when it found the disk, or when it last reread the table, as a clone
    /// asks of it before its assembly: the assembly neither compares them with the table nor asks
    /// the kernel to reread it. Its first partition holds opaque bytes, which the assembly
    /// returns through [`Root::first_partition`], and never probes or mounts. Each later
    /// partition, from partition 2 on, holds a layer, an ext4 file system that the assembly
    /// mounts at `layers/pN`, for the partition's number N, `ro,nosuid,nodev`, with
    /// `noload,errors=remount-ro`: it never loads or replays the journal, whose scan can hang
    /// even when the superblock does not request recovery, and overrides a superblock's
    /// request to panic on an ext4 error.
    /// It refuses, before it mounts anything, a layer whose superblock has the `needs_recovery`
    /// feature: the layer's read-only device can't take the recovery, and suppressing journal
    /// loading must not silently admit such a layer. The overlay
    /// stacks the layers with partition 2's at its base and the last partition's at its top. A
    /// disk has at least one layer, as [`nvx::gpt`](crate::gpt) requires.
    Gpt {
        /// What the caller expects of the disk beyond its format: its GUID, and its partitions'
        /// unique GUIDs and sizes, each if the caller knows it.
        expected: Expected,
        /// The most bytes that the first partition may hold: a larger first partition is refused
        /// before any of it is read.
        first_partition_bound: usize,
    },
}

/// The file system that holds the overlay's upper and work directories, two separate
/// directories on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Upper {
    /// A whole-device ext4 file system on the scratch device, which an assembly mounts
    /// `rw,nosuid,nodev` at `scratch`, and whose directories persist across boots.
    Scratch {
        /// The scratch device.
        device: Device,
        /// Whether the file system may have a journal.
        journal: Journal,
    },
    /// A tmpfs at the kernel's default size, which an assembly mounts `nosuid,nodev` at `tmpfs`,
    /// for a VM that has no scratch device.
    Tmpfs,
}

impl Upper {
    /// Returns the upper's kind.
    pub const fn kind(self) -> UpperKind {
        match self {
            Self::Scratch { .. } => UpperKind::Scratch,
            Self::Tmpfs => UpperKind::Tmpfs,
        }
    }
}

/// The kind of a root's upper, which [`Root::upper`] reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum UpperKind {
    /// An ext4 scratch, on the scratch device.
    Scratch,
    /// A tmpfs.
    Tmpfs,
}

/// Whether an ext4 scratch may have a journal, which [`assemble`] checks in the feature flags of
/// its superblock before it mounts it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Journal {
    /// The scratch may have a journal or none, so the assembly reads nothing to check it.
    Allowed,
    /// The scratch may not have a journal: the assembly refuses a scratch whose ext4 superblock
    /// has the `has_journal` feature, and one that holds no ext4 superblock.
    Forbidden,
}

/// The overlay's mode, which selects its one option set. The kernel's refusal of the set fails
/// the assembly, which never falls back to another set, and so does an overlay that the kernel
/// mounts without the set in effect.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OverlayMode {
    /// `metacopy=on,xino=on`, the default, for lowers whose overlay xattrs the caller trusts: a
    /// change of a lower file's metadata copies up its metadata alone, renaming a lower directory
    /// records a redirect, and inode numbers stay unique across the layers.
    #[default]
    TrustedLowers,
    /// `metacopy=off,redirect_dir=nofollow`, for lowers that may carry untrusted overlay xattrs:
    /// the overlay neither creates metacopy and redirect xattrs nor follows a lower's, so a
    /// copy-up copies a file's data too, and renaming a lower directory fails with `EXDEV`.
    UntrustedLowers,
}

impl OverlayMode {
    /// Returns the mode's option set, which follows the overlay's directories in its options.
    pub const fn options(self) -> &'static str {
        match self {
            Self::TrustedLowers => "metacopy=on,xino=on",
            Self::UntrustedLowers => "metacopy=off,redirect_dir=nofollow",
        }
    }
}

/// Assembles the layered root of `assembly`, as the [module](self) describes, and returns the
/// root that owns its mounts. It checks everything before it mounts anything. In order, it:
///
/// 1. checks the runtime directory's path;
/// 2. creates the runtime directory, as `mkdir -p` does, and refuses it if a symbolic link leads
///    through it;
/// 3. creates the mount points of an EROFS image, `layers/distro`, of the upper, `scratch` or
///    `tmpfs`, and of the root, `rootfs`, a component at a time, and refuses a component that
///    exists as anything but a directory, a symbolic link included; takes the runtime directory's
///    exclusive lock, which the root holds from then on, and refuses the runtime directory if
///    another root holds it, or if one of these, the other upper's, any entry of `layers`,
///    `shares`, where a root mounts an aggregate live share, or `hostfs/root`, where it mounts the
///    host export, is a mount point already, whether of a file system or of a bind mount of a
///    directory;
/// 4. finds the distro device, polling for it every 50 ms until `timeout` passes, checks that it
///    is read-only and, if the caller expects a size, of that size, and flushes its buffers;
/// 5. for an EROFS image, checks the expected UUID, if any, in its superblock, and reads nothing
///    else; for a GPT disk, reads and checks its partition table as
///    [`guest::gpt::read_disk`](super::gpt::read_disk) does, reads the first partition, checks
///    that the node of each layer's partition, which devtmpfs created when the kernel found the
///    disk, exists, checks in each layer's superblock that it is ext4 and lacks the
///    `needs_recovery` feature, and creates the layers' mount points, `layers/pN`, as step 3
///    does;
/// 6. for an ext4 scratch, finds, checks, and flushes the scratch device as step 4 does, except
///    that it must be writable, and, if the caller forbids a journal, checks that its superblock
///    lacks the `has_journal` feature;
/// 7. checks that the overlay's options fit in [`MOUNT_OPTIONS_LIMIT`] bytes;
/// 8. mounts the layers: an EROFS image read-only, or a GPT disk's ext4 layers read-only with
///    `nosuid` and `nodev`;
/// 9. mounts the upper with `nosuid` and `nodev`: the scratch's ext4 file system, or a tmpfs
///    whose root directory has mode 0755;
/// 10. creates the upper's `upper` and `work` directories unless they exist, with mode 0755
///     whatever the process's umask, since the overlay's root directory takes the upper's mode;
/// 11. mounts the overlay at `rootfs`, `nosuid` and `nodev`, with the layers as its lower
///     directories, the top first, and the option set of the overlay mode;
/// 12. and checks, in the kernel's mount table, that the overlay is writable and has each option
///     of the set in effect, where an option that the table doesn't show has its default, which
///     a parameter of the overlay module holds. Without failing the mount, the kernel mounts an
///     overlay read-only when it can't create its work directory, and drops metadata copy-up and
///     redirects when the upper can't hold the overlay's extended attributes.
///
/// A failure unmounts what the assembly mounted, as [`Root::teardown`] does, and reports both.
pub fn assemble(assembly: &Assembly) -> Result<Root, AssemblyError> {
    assemble_until(assembly, None)
}

/// Assembles the layered root of `assembly`, as [`assemble`] does, but waits for each device until
/// `deadline` at the latest, if any, however long `assembly`'s timeout allows: a device that has
/// not appeared by then is missing, as one that has not appeared by the timeout is.
pub(crate) fn assemble_until(
    assembly: &Assembly,
    deadline: Option<Instant>,
) -> Result<Root, AssemblyError> {
    assemble_with(
        Box::new(SystemKernel),
        &Paths::default(),
        assembly,
        deadline,
        Contents::Kept,
    )
}

/// Assembles the root of `assembly` as [`assemble_until`] does, waiting for each device until
/// `deadline` at the latest, if any, but with a fresh upper, as a clone's application asks: once
/// it mounted the upper, and before it creates either directory, it refuses the upper's `upper` or
/// `work` directory if it exists and is not empty, with [`Error::NotFresh`]. A cold sandbox's
/// scratch keeps its upper across boots, but a clone's root must start as its image alone, since
/// the overlay would show what an earlier root wrote. A tmpfs, which the assembly mounts empty,
/// always passes.
///
/// The check is bounded, and reads no file's content: of each directory, it reads the status,
/// and at most one entry besides `.` and `..`, and it opens nothing below it.
pub(crate) fn assemble_fresh(
    assembly: &Assembly,
    deadline: Option<Instant>,
) -> Result<Root, AssemblyError> {
    assemble_with(
        Box::new(SystemKernel),
        &Paths::default(),
        assembly,
        deadline,
        Contents::Fresh,
    )
}

/// Asks the kernel to reread the partition table of the GPT disk on the distro device that the
/// VMM attached at `distro`'s MMIO window, once the device appeared, polling for it as an
/// assembly does, for as long as `timeout` allows, but never past `deadline`, if any. A clone's
/// application asks this before it assembles a root of the disk: the kernel then drops the block
/// devices of the partitions that it created when it found the template's placeholder, and
/// creates those of the clone's disk, and devtmpfs their nodes, from the same read-only bytes that
/// the assembly reads and checks next, as at a cold boot. It checks nothing else of the device,
/// which the assembly checks.
///
/// It needs `CAP_SYS_ADMIN`, and the kernel refuses it while one of the disk's partitions is
/// open, as a mounted one is.
pub(crate) fn reread_partitions(
    distro: Device,
    timeout: Duration,
    deadline: Option<Instant>,
) -> Result<(), Error> {
    block::reread(
        &mut SystemKernel,
        &Paths::default(),
        distro,
        timeout,
        deadline,
    )
    .map(drop)
}

/// What an assembly accepts in the upper's `upper` and `work` directories, which it creates
/// unless they exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Contents {
    /// Whatever they hold, which the root keeps, as [`assemble`] accepts: a cold sandbox's
    /// scratch keeps its upper across boots.
    Kept,
    /// Nothing: each must be missing or empty, as [`assemble_fresh`] requires.
    Fresh,
}

/// Implements [`assemble_until`] and [`assemble_fresh`] with `kernel`, and with the kernel's
/// interfaces at `paths`, accepting the upper's `contents`.
fn assemble_with(
    kernel: Box<dyn Kernel>,
    paths: &Paths,
    assembly: &Assembly,
    deadline: Option<Instant>,
    contents: Contents,
) -> Result<Root, AssemblyError> {
    let runtime = assembly.runtime.as_os_str().as_bytes();
    if below_its_root(&assembly.runtime, true).is_none()
        || runtime.iter().any(|byte| RUNTIME_RESERVED.contains(byte))
    {
        return Err(AssemblyError {
            error: Error::Runtime {
                path: assembly.runtime.clone(),
            },
            cleanup: None,
        });
    }
    let mut root = Root {
        kernel,
        runtime: assembly.runtime.clone(),
        path: assembly.runtime.join(ROOT_DIRECTORY),
        overlay: None,
        lowers: Vec::new(),
        upper: None,
        upper_kind: assembly.upper.kind(),
        overlay_mode: assembly.overlay,
        first_partition: None,
        shares: Vec::new(),
        mapper: None,
        mapped: Vec::new(),
        latched: false,
        reservation: None,
    };
    match root.mount_layers(paths, assembly, deadline, contents) {
        Ok(()) => Ok(root),
        Err(error) => Err(AssemblyError {
            error,
            cleanup: root.unmount().err(),
        }),
    }
}

/// A read-only layer of an assembled root, one of the overlay's lower directories.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lower {
    /// The layer's mount.
    mount: Mount,
    /// The node of the layer's block device.
    device: PathBuf,
    /// The layer's mount point.
    path: PathBuf,
    /// The GPT partition that holds the layer, if a GPT disk does.
    partition: Option<Partition>,
}

impl Lower {
    /// Returns the layer's mount: [`Mount::Distro`] for an EROFS image, or [`Mount::Layer`] for a
    /// layer of a GPT disk.
    pub fn mount(&self) -> &Mount {
        &self.mount
    }

    /// Returns the node of the layer's block device: the distro device that holds an EROFS image,
    /// or the block device of a GPT disk's partition.
    pub fn device(&self) -> &Path {
        &self.device
    }

    /// Returns the layer's mount point, below the runtime directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the GPT partition that holds the layer, with its number, GUIDs, and extent, or
    /// `None` for an EROFS image.
    pub fn partition(&self) -> Option<&Partition> {
        self.partition.as_ref()
    }
}

/// The handle of an assembled layered root, which owns its mounts, those of the assembly and
/// those of its live shares, host export, and mappings, and reports what the assembly mounted.
///
/// [`teardown`](Self::teardown) unmounts them and reports each failure. Dropping the root
/// unmounts them the same way, and ignores failures. Until then, the root holds the lock of its
/// runtime directory, where no other root assembles.
pub struct Root {
    /// The kernel that mounts and unmounts.
    kernel: Box<dyn Kernel>,
    /// The runtime directory.
    runtime: PathBuf,
    /// The overlay's mount point, which is the root.
    path: PathBuf,
    /// The overlay's mount point, while it is mounted.
    overlay: Option<PathBuf>,
    /// The mounted layers, in the order of their mounts: the base first.
    lowers: Vec<Lower>,
    /// The upper's mount and mount point, while it is mounted.
    upper: Option<(Mount, PathBuf)>,
    /// The upper's kind.
    upper_kind: UpperKind,
    /// The overlay's mode.
    overlay_mode: OverlayMode,
    /// The bytes of a GPT disk's first partition, without their trailing padding.
    first_partition: Option<Vec<u8>>,
    /// The mounts of live shares and of the host export that the root unmounts by path, and their
    /// mount points, in mount order: an aggregate's, the export's, and a share's that stays at
    /// `staging` because its unmount failed.
    shares: Vec<(Mount, PathBuf)>,
    /// The mapper of the live shares and mappings in the root, once the root has mounted one.
    mapper: Option<Mapper>,
    /// What each of the mapper's mappings mounts, in the mapper's order, then what the one that
    /// a rename moved and that the mapper failed to detach mounts, if any.
    mapped: Vec<Mount>,
    /// Whether a share, the export, or a mapping failed, after which the root refuses every later
    /// one.
    latched: bool,
    /// The exclusive lock of the runtime directory, from the start of the assembly; dropping the
    /// root releases it after the unmounts.
    reservation: Option<sys::FileLock>,
}

impl Root {
    /// Returns the root: the overlay's mount point, `rootfs` in the runtime directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the runtime directory.
    pub fn runtime(&self) -> &Path {
        &self.runtime
    }

    /// Returns whether a live share, the host export, or a mapping failed, after which the root
    /// refuses every later one, so that its caller starts no workload in it.
    pub fn is_latched(&self) -> bool {
        self.latched
    }

    /// Returns the bytes of a GPT disk's first partition: all of its bytes, less its trailing NUL
    /// bytes and ASCII whitespace, as opaque bytes that need not be UTF-8 and that the root never
    /// interprets. Returns `None` for an EROFS image.
    pub fn first_partition(&self) -> Option<&[u8]> {
        self.first_partition.as_deref()
    }

    /// Returns the overlay's layers, the base first: an EROFS image, or the layers of a GPT disk
    /// in the order of their partitions, from partition 2's, the base, to the last partition's,
    /// the top.
    pub fn lowers(&self) -> &[Lower] {
        &self.lowers
    }

    /// Returns the kind of the root's upper.
    pub fn upper(&self) -> UpperKind {
        self.upper_kind
    }

    /// Returns the overlay's mode, whose option set, [`OverlayMode::options`], the assembly found
    /// in effect.
    pub fn overlay(&self) -> OverlayMode {
        self.overlay_mode
    }

    /// Unmounts the root's mounts, in order: a live share or a mapping that a rename moved and
    /// that its mapper failed to detach, which the mapper detaches, the live shares and mappings
    /// in reverse order, the mounts of an aggregate live share and of the host export, the later
    /// first, the overlay, the layers from the top down, and the upper.
    ///
    /// Each unmount is attempted, whatever failed before, without detaching a busy mount but the
    /// moved one, and every one that failed is reported, with the mount that it leaves in place.
    /// A [`SourceRoot`](super::mappings::SourceRoot) of the export that the caller still holds
    /// keeps the export busy.
    ///
    /// The file system of an ext4 scratch is synced to the scratch device before the scratch is
    /// unmounted. A mount namespace that copied the root's mounts, such as one that a process
    /// entered while the root was mounted, keeps the overlay and the scratch mounted after this
    /// namespace unmounts them, and the file system then writes nothing back until the last copy
    /// goes, which may be after the VM powers off. So a teardown that succeeds leaves every write
    /// to the root on the scratch device. A scratch whose sync fails is not unmounted, and the
    /// failure is reported as its own.
    pub fn teardown(mut self) -> Result<(), TeardownError> {
        self.unmount()
    }

    /// Implements [`assemble`]'s steps after the first, waiting for each device until `deadline`
    /// at the latest, if any, and accepting the upper's `contents`.
    fn mount_layers(
        &mut self,
        paths: &Paths,
        assembly: &Assembly,
        deadline: Option<Instant>,
        contents: Contents,
    ) -> Result<(), Error> {
        let runtime = self.runtime.clone();
        fs::create_dir_all(&runtime).map_err(|error| Error::MountPoint {
            path: runtime.clone(),
            error,
        })?;
        // The path is in canonical form already, so it differs from the canonical one only
        // where a symbolic link leads through it.
        match fs::canonicalize(&runtime) {
            Ok(canonical) if canonical == runtime => {}
            Ok(_) => return Err(Error::Runtime { path: runtime }),
            Err(error) => {
                return Err(Error::MountPoint {
                    path: runtime,
                    error,
                });
            }
        }
        let mut points = Vec::new();
        match assembly.image {
            Image::Erofs { .. } => points.push(create_below(&runtime, DISTRO_DIRECTORY)?),
            Image::Gpt { .. } => {
                create_below(&runtime, LAYERS_DIRECTORY)?;
            }
        }
        let upper_point = create_below(
            &runtime,
            match assembly.upper.kind() {
                UpperKind::Scratch => SCRATCH_DIRECTORY,
                UpperKind::Tmpfs => TMPFS_DIRECTORY,
            },
        )?;
        points.push(upper_point.clone());
        points.push(create_below(&runtime, ROOT_DIRECTORY)?);
        self.reservation = Some(reserve(&runtime)?);
        for point in &points {
            self.check_unoccupied(point, false)?;
        }
        // A root whose teardown failed may have left mounted an upper of the other kind, an
        // aggregate share, or the host export, which this root mounts only later.
        let other_upper = match assembly.upper.kind() {
            UpperKind::Scratch => TMPFS_DIRECTORY,
            UpperKind::Tmpfs => SCRATCH_DIRECTORY,
        };
        for point in [other_upper, AGGREGATE_DIRECTORY, EXPORT_DIRECTORY] {
            self.check_unoccupied(&runtime.join(point), true)?;
        }
        // Or a layer of either image, at any mount point below `layers`: an EROFS image's, or a
        // GPT disk's, which may have had more layers than this root's.
        let layers = runtime.join(LAYERS_DIRECTORY);
        let mut entries = Vec::new();
        let listing = fs::read_dir(&layers).and_then(|listing| {
            for entry in listing {
                entries.push(entry?.path());
            }
            Ok(())
        });
        listing.map_err(|error| Error::MountPoint {
            path: layers.clone(),
            error,
        })?;
        entries.sort();
        for entry in &entries {
            self.check_unoccupied(entry, true)?;
        }
        let device = block::prepare(
            self.kernel.as_mut(),
            paths,
            Role::Distro,
            assembly.distro,
            assembly.timeout,
            deadline,
        )?;
        let lowers = match &assembly.image {
            Image::Erofs { uuid } => {
                check_erofs_uuid(&device, *uuid)?;
                vec![Lower {
                    mount: Mount::Distro,
                    device,
                    path: runtime.join(DISTRO_DIRECTORY),
                    partition: None,
                }]
            }
            Image::Gpt {
                expected,
                first_partition_bound,
            } => self.gpt_layers(device, expected, *first_partition_bound)?,
        };
        let scratch = match assembly.upper {
            Upper::Scratch { device, journal } => {
                let device = block::prepare(
                    self.kernel.as_mut(),
                    paths,
                    Role::Scratch,
                    device,
                    assembly.timeout,
                    deadline,
                )?;
                if journal == Journal::Forbidden {
                    check_no_journal(&device)?;
                }
                Some(device)
            }
            Upper::Tmpfs => None,
        };
        let upper = upper_point.join(UPPER_DIRECTORY);
        let work = upper_point.join(WORK_DIRECTORY);
        let options = overlay_options(&lowers, &upper, &work, assembly.overlay)?;
        for lower in lowers {
            let (filesystem, flags, options) = match lower.partition {
                None => (b"erofs".as_slice(), MountFlags::MS_RDONLY, None),
                Some(_) => (
                    b"ext4".as_slice(),
                    MountFlags::MS_RDONLY | MountFlags::MS_NOSUID | MountFlags::MS_NODEV,
                    Some(LAYER_OPTIONS),
                ),
            };
            self.mount_at(
                &lower.mount,
                Some(lower.device.as_os_str().as_bytes()),
                &lower.path,
                Some(filesystem),
                flags,
                options,
            )?;
            self.lowers.push(lower);
        }
        // Neither set-ID programs nor device nodes of the upper take effect.
        let flags = MountFlags::MS_NOSUID | MountFlags::MS_NODEV;
        let mount = match scratch {
            Some(device) => {
                self.mount_at(
                    &Mount::Scratch,
                    Some(device.as_os_str().as_bytes()),
                    &upper_point,
                    Some(b"ext4"),
                    flags,
                    None,
                )?;
                Mount::Scratch
            }
            None => {
                self.mount_at(
                    &Mount::Tmpfs,
                    Some(b"tmpfs"),
                    &upper_point,
                    Some(b"tmpfs"),
                    flags,
                    Some(TMPFS_OPTIONS),
                )?;
                Mount::Tmpfs
            }
        };
        self.upper = Some((mount, upper_point));
        if contents == Contents::Fresh {
            for directory in [&upper, &work] {
                check_fresh(directory)?;
            }
        }
        for directory in [&upper, &work] {
            create_scratch_directory(directory).map_err(|error| Error::MountPoint {
                path: directory.clone(),
                error,
            })?;
        }
        let root = self.path.clone();
        // Neither set-ID programs nor device nodes of the layers or the upper take effect.
        self.mount_at(
            &Mount::Overlay,
            Some(b"overlay"),
            &root,
            Some(b"overlay"),
            flags,
            Some(&options),
        )?;
        self.overlay = Some(root.clone());
        self.check_overlay(paths, &root, assembly.overlay)
    }

    /// Checks that the kernel mounted the overlay at `root` as the assembly asked: writable, and
    /// with each option of `mode`'s set in effect. The kernel's mount table shows only those of
    /// the overlay's options that differ from their defaults, so an option that it doesn't show
    /// has the default that a parameter of the overlay module holds.
    fn check_overlay(
        &mut self,
        paths: &Paths,
        root: &Path,
        mode: OverlayMode,
    ) -> Result<(), Error> {
        let options = self
            .kernel
            .mount_options(root)
            .map_err(|error| Error::OverlayCheck {
                path: root.to_owned(),
                error,
            })?;
        let mut options = options.split(',');
        let access = options.next().unwrap_or_default();
        if access != "rw" {
            return Err(Error::OverlayFallback {
                requested: "rw".to_owned(),
                effective: access.to_owned(),
            });
        }
        let shown: Vec<(&str, &str)> = options
            .filter_map(|option| option.split_once('='))
            .collect();
        for option in mode.options().split(',') {
            let (key, value) = option.split_once('=').unwrap_or((option, ""));
            let effective = match shown.iter().find(|(shown, _)| *shown == key) {
                Some(&(_, effective)) => Some(effective),
                None => overlay_default(paths, key)?,
            };
            if effective != Some(value) {
                return Err(Error::OverlayFallback {
                    requested: option.to_owned(),
                    effective: effective.map_or_else(
                        || format!("no {key}"),
                        |effective| format!("{key}={effective}"),
                    ),
                });
            }
        }
        Ok(())
    }

    /// Reads and checks the partition table of the GPT disk on the distro device `device`, against
    /// `expected`, keeps the bytes of its first partition, at most `first_partition_bound`, and
    /// returns its layers, in the order of their partitions, on the block devices that the kernel
    /// created for them, with their mount points, which it creates below the runtime directory.
    fn gpt_layers(
        &mut self,
        device: PathBuf,
        expected: &Expected,
        first_partition_bound: usize,
    ) -> Result<Vec<Lower>, Error> {
        let (disk, first_partition) = {
            let file = File::open(&device).map_err(|error| Error::Device {
                role: Role::Distro,
                step: DeviceStep::Open,
                device: device.clone(),
                error,
            })?;
            let disk = self
                .kernel
                .read_disk(&device, &file, expected)
                .map_err(|error| Error::Disk {
                    device: device.clone(),
                    error,
                })?;
            let first_partition = disk
                .read_first_partition(&file, first_partition_bound)
                .map_err(|error| Error::FirstPartition {
                    device: device.clone(),
                    error,
                })?;
            (disk, first_partition)
        };
        // The first partition is the one numbered 1, and every other partition holds a layer.
        let layers: Vec<Partition> = disk
            .partitions()
            .iter()
            .filter(|partition| partition.number() > 1)
            .copied()
            .collect();
        let numbers: Vec<u8> = layers.iter().map(Partition::number).collect();
        let nodes = block::partition_nodes(&device, &numbers)?;
        let mut lowers = Vec::with_capacity(layers.len());
        for (partition, node) in layers.into_iter().zip(nodes) {
            let number = partition.number();
            check_layer(number, &node)?;
            let path = create_below(&self.runtime, &format!("{LAYERS_DIRECTORY}/p{number}"))?;
            self.check_unoccupied(&path, false)?;
            lowers.push(Lower {
                mount: Mount::Layer { number },
                device: node,
                path,
                partition: Some(partition),
            });
        }
        self.first_partition = Some(first_partition);
        Ok(lowers)
    }

    /// Refuses the directory `path` where a file system, or a bind mount of a directory, is
    /// mounted already, and one that can't be checked, unless `may_be_missing` and it doesn't
    /// exist.
    fn check_unoccupied(&mut self, path: &Path, may_be_missing: bool) -> Result<(), Error> {
        match self.kernel.is_mount_point(path) {
            Ok(false) => Ok(()),
            Ok(true) => Err(Error::Occupied {
                path: path.to_owned(),
            }),
            Err(error) if may_be_missing && error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::MountPoint {
                path: path.to_owned(),
                error,
            }),
        }
    }

    /// Mounts `source` at `target` with `filesystem`, `flags`, and the file system's own options
    /// `data`, and reports a failure as one to mount `mount`.
    fn mount_at(
        &mut self,
        mount: &Mount,
        source: Option<&[u8]>,
        target: &Path,
        filesystem: Option<&[u8]>,
        flags: MountFlags,
        data: Option<&[u8]>,
    ) -> Result<(), Error> {
        let mounted = (|| {
            let source = source.map(sys::c_string).transpose()?;
            let filesystem = filesystem.map(sys::c_string).transpose()?;
            let data = data.map(sys::c_string).transpose()?;
            let target = sys::path_c_string(target)?;
            self.kernel.mount(
                source.as_deref(),
                &target,
                filesystem.as_deref(),
                flags,
                data.as_deref(),
            )
        })();
        mounted.map_err(|error| Error::Mount {
            mount: mount.clone(),
            error: error.into(),
        })
    }

    /// Unmounts what the root has mounted, as [`teardown`](Self::teardown) describes, and
    /// forgets it, failed unmounts included.
    fn unmount(&mut self) -> Result<(), TeardownError> {
        let mut failures = Vec::new();
        // The mapper's descriptor of the root keeps the overlay busy, so it goes first.
        if let Some(mut mapper) = self.mapper.take() {
            let mapped = std::mem::take(&mut self.mapped);
            for (index, failure) in mapper.unmount_with(self.kernel.as_mut()) {
                let target = failure.target();
                let mount = mapped
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| Mount::Mapping {
                        target: target.to_owned(),
                    });
                failures.push(Unmount {
                    mount,
                    path: self.path.join(target.strip_prefix("/").unwrap_or(target)),
                    step: Step::Unmount,
                    error: share::io_error(failure.error()),
                });
            }
        }
        let mut mounts: Vec<(Mount, PathBuf)> = self.shares.drain(..).rev().collect();
        mounts.extend(self.overlay.take().map(|path| (Mount::Overlay, path)));
        mounts.extend(
            self.lowers
                .drain(..)
                .rev()
                .map(|lower| (lower.mount, lower.path)),
        );
        mounts.extend(self.upper.take());
        for (mount, path) in mounts {
            let target = sys::path_c_string(&path);
            let synced = match (&mount, &target) {
                (Mount::Scratch, Ok(target)) => self.kernel.sync_file_system(target),
                _ => Ok(()),
            };
            let (step, outcome) = match synced {
                Ok(()) => (
                    Step::Unmount,
                    target.and_then(|target| self.kernel.unmount(&target)),
                ),
                Err(error) => (Step::Sync, Err(error)),
            };
            if let Err(error) = outcome {
                failures.push(Unmount {
                    mount,
                    path,
                    step,
                    error: error.into(),
                });
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(TeardownError { failures })
        }
    }
}

impl Drop for Root {
    /// Unmounts what the root has mounted, as [`Root::teardown`] does, and ignores failures.
    fn drop(&mut self) {
        let _ = self.unmount();
    }
}

impl fmt::Debug for Root {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Root")
            .field("runtime", &self.runtime)
            .field("path", &self.path)
            .field("overlay", &self.overlay)
            .field("overlay_mode", &self.overlay_mode)
            .field("lowers", &self.lowers)
            .field("upper", &self.upper)
            .field("upper_kind", &self.upper_kind)
            .field(
                "first_partition_bytes",
                &self.first_partition.as_ref().map(Vec::len),
            )
            .field("shares", &self.shares)
            .field("mapped", &self.mapped)
            .field("latched", &self.latched)
            .finish_non_exhaustive()
    }
}

/// Checks the EROFS image on the distro device `device` for the UUID `expected`, if the caller
/// expects one, in its superblock, and reads nothing else.
fn check_erofs_uuid(device: &Path, expected: Option<Uuid>) -> Result<(), Error> {
    let Some(expected) = expected else {
        return Ok(());
    };
    match block::erofs_uuid(device) {
        Ok(Some(actual)) if actual == expected => Ok(()),
        Ok(Some(actual)) => Err(Error::Uuid { expected, actual }),
        Ok(None) => Err(Error::NotErofs {
            device: device.to_owned(),
        }),
        Err(error) => Err(Error::Device {
            role: Role::Distro,
            step: DeviceStep::Superblock,
            device: device.to_owned(),
            error,
        }),
    }
}

/// Checks that the ext4 file system on the scratch device `device` has no journal, in its
/// superblock's feature flags, and reads nothing else.
fn check_no_journal(device: &Path) -> Result<(), Error> {
    match block::ext4_features(device) {
        Ok(Some(features)) if !features.journal => Ok(()),
        Ok(Some(_)) => Err(Error::Journal {
            device: device.to_owned(),
        }),
        Ok(None) => Err(Error::NotExt4 {
            device: device.to_owned(),
        }),
        Err(error) => Err(Error::Device {
            role: Role::Scratch,
            step: DeviceStep::Superblock,
            device: device.to_owned(),
            error,
        }),
    }
}

/// Checks that the layer in partition `number` of a GPT disk, on the block device `device`, holds
/// an ext4 file system whose superblock does not request journal recovery, and
/// reads nothing else.
///
/// The layer's device is read-only, so it can't take journal recovery. Reject the recovery bit
/// explicitly because our `noload` mount option would otherwise let the kernel skip it.
fn check_layer(number: u8, device: &Path) -> Result<(), Error> {
    match block::ext4_features(device) {
        Ok(Some(features)) if !features.recovery => Ok(()),
        Ok(Some(_)) => Err(Error::Recovery {
            number,
            device: device.to_owned(),
        }),
        Ok(None) => Err(Error::NotExt4Layer {
            number,
            device: device.to_owned(),
        }),
        Err(error) => Err(Error::Device {
            role: Role::Distro,
            step: DeviceStep::Superblock,
            device: device.to_owned(),
            error,
        }),
    }
}

/// Returns the overlay's options: its lower directories, the mount points of `lowers` from the
/// top down, its `upper` and `work` directories, and the option set of `mode`, or fails if they
/// are longer than [`MOUNT_OPTIONS_LIMIT`].
fn overlay_options(
    lowers: &[Lower],
    upper: &Path,
    work: &Path,
    mode: OverlayMode,
) -> Result<Vec<u8>, Error> {
    let mut options = b"lowerdir=".to_vec();
    for (index, lower) in lowers.iter().rev().enumerate() {
        if index > 0 {
            options.push(b':');
        }
        options.extend_from_slice(lower.path.as_os_str().as_bytes());
    }
    for (name, directory) in [(&b",upperdir="[..], upper), (b",workdir=", work)] {
        options.extend_from_slice(name);
        options.extend_from_slice(directory.as_os_str().as_bytes());
    }
    options.push(b',');
    options.extend_from_slice(mode.options().as_bytes());
    if options.len() > MOUNT_OPTIONS_LIMIT {
        return Err(Error::OverlayOptions {
            length: options.len(),
        });
    }
    Ok(options)
}

/// Returns the default of the overlay's option `key`, which a parameter of the overlay module
/// holds, as the kernel shows the option's value, or `None` for an option without one.
fn overlay_default(paths: &Paths, key: &str) -> Result<Option<&'static str>, Error> {
    let parameter = |name: &str| {
        let path = paths.sys.join(OVERLAY_PARAMETERS).join(name);
        match fs::read(&path) {
            Ok(value) if value == b"Y\n" => Ok(true),
            Ok(value) if value == b"N\n" => Ok(false),
            Ok(_) => Err(Error::OverlayCheck {
                path,
                error: Errno::EPROTO.into(),
            }),
            Err(error) => Err(Error::OverlayCheck { path, error }),
        }
    };
    let default = match key {
        "metacopy" if parameter("metacopy")? => "on",
        "metacopy" => "off",
        "xino" if parameter("xino_auto")? => "auto",
        "xino" => "off",
        "redirect_dir" if parameter("redirect_dir")? => "on",
        "redirect_dir" if parameter("redirect_always_follow")? => "follow",
        "redirect_dir" => "nofollow",
        _ => return Ok(None),
    };
    Ok(Some(default))
}

/// Takes the exclusive lock of the runtime directory `runtime`, without waiting for another root
/// that holds it.
fn reserve(runtime: &Path) -> Result<sys::FileLock, Error> {
    sys::path_c_string(runtime)
        .and_then(|path| sys::try_lock_directory(&path))
        .map_err(|error| match error {
            Errno::EWOULDBLOCK => Error::Reserved {
                path: runtime.to_owned(),
            },
            error => Error::MountPoint {
                path: runtime.to_owned(),
                error: error.into(),
            },
        })
}

/// Creates the directory `relative` below the directory `base`, a component at a time, each
/// unless a directory, and not a symbolic link, is there already, and returns its path.
fn create_below(base: &Path, relative: &str) -> Result<PathBuf, Error> {
    let mut path = base.to_path_buf();
    for component in relative.split('/') {
        path.push(component);
        create_directory(&path).map_err(|error| Error::MountPoint {
            path: path.clone(),
            error,
        })?;
    }
    Ok(path)
}

/// Creates the directory `path` unless a directory, and not a symbolic link, is there already,
/// and returns whether it created it.
fn create_directory(path: &Path) -> io::Result<bool> {
    match fs::create_dir(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if fs::symlink_metadata(path)?.is_dir() {
                Ok(false)
            } else {
                Err(Errno::ENOTDIR.into())
            }
        }
        Err(error) => Err(error),
    }
}

/// Creates the scratch's directory `path` as [`create_directory`] does, with
/// [`SCRATCH_DIRECTORY_MODE`] whatever the process's umask. A directory that is there already,
/// which the scratch keeps across boots, keeps its mode.
fn create_scratch_directory(path: &Path) -> io::Result<()> {
    if create_directory(path)? {
        fs::set_permissions(path, fs::Permissions::from_mode(SCRATCH_DIRECTORY_MODE))?;
    }
    Ok(())
}

/// Refuses the upper's directory `path`, its `upper` or `work`, if it exists and is not empty, as
/// a fresh upper's may not be, and anything there but a directory, a symbolic link included, as
/// [`create_directory`] does. It reads the status of `path`, and, of a directory, at most one
/// entry besides `.` and `..`, in whichever order the file system lists them; it opens nothing
/// below it.
fn check_fresh(path: &Path) -> Result<(), Error> {
    let failed = |error: io::Error| Error::MountPoint {
        path: path.to_owned(),
        error,
    };
    match fs::symlink_metadata(path) {
        Ok(status) if status.is_dir() => {}
        Ok(_) => return Err(failed(Errno::ENOTDIR.into())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(failed(error)),
    }
    match fs::read_dir(path).and_then(holds_entry) {
        Ok(false) => Ok(()),
        Ok(true) => Err(Error::NotFresh {
            path: path.to_owned(),
        }),
        Err(error) => Err(failed(error)),
    }
}

/// Returns whether the directory listing `entries`, which lists no `.` or `..`, holds an entry,
/// from its first alone.
fn holds_entry<T>(mut entries: impl Iterator<Item = io::Result<T>>) -> io::Result<bool> {
    entries.next().transpose().map(|entry| entry.is_some())
}

/// A mount of a layered root.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Mount {
    /// The EROFS image, at `layers/distro` in the runtime directory.
    Distro,
    /// The layer in a partition of a GPT disk, at `layers/pN` in the runtime directory, for its
    /// partition's number N.
    Layer {
        /// The number of the layer's partition: 2 for the base, and more for each layer above.
        number: u8,
    },
    /// The ext4 scratch, at `scratch` in the runtime directory.
    Scratch,
    /// The tmpfs upper, at `tmpfs` in the runtime directory.
    Tmpfs,
    /// The overlay, which is the root, at `rootfs` in the runtime directory.
    Overlay,
    /// A live share, at its target in the root.
    Share {
        /// The share's virtio-fs tag.
        tag: String,
        /// The share's target in the root.
        target: PathBuf,
    },
    /// An aggregate live share, at `shares` in the runtime directory.
    Aggregate {
        /// The aggregate's virtio-fs tag.
        tag: String,
    },
    /// The host export, at `hostfs/root` in the runtime directory.
    Export {
        /// The export's virtio-fs tag.
        tag: String,
    },
    /// A child of an aggregate live share, bound at its target in the root.
    Child {
        /// The child's name in the aggregate.
        name: String,
        /// The child's target in the root.
        target: PathBuf,
    },
    /// A caller's host mapping, at its target in the root.
    Mapping {
        /// The mapping's target in the root.
        target: PathBuf,
    },
}

impl fmt::Display for Mount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Distro => formatter.write_str("the distro layer"),
            Self::Layer { number } => write!(formatter, "the layer in partition {number}"),
            Self::Scratch => formatter.write_str("the scratch"),
            Self::Tmpfs => formatter.write_str("the tmpfs upper"),
            Self::Overlay => formatter.write_str("the overlay"),
            Self::Share { tag, target } => {
                write!(formatter, "the live share {tag} at {}", target.display())
            }
            Self::Aggregate { tag } => write!(formatter, "the aggregate live share {tag}"),
            Self::Export { tag } => write!(formatter, "the host export {tag}"),
            Self::Child { name, target } => {
                write!(
                    formatter,
                    "the live share child {name} at {}",
                    target.display()
                )
            }
            Self::Mapping { target } => write!(formatter, "the mapping at {}", target.display()),
        }
    }
}

/// Why a root operation failed. Its text names what failed, and the system's error, if any.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// A step of [`bootstrap`] failed.
    Bootstrap {
        /// The step that failed.
        step: BootstrapStep,
        /// The step's error.
        error: io::Error,
    },
    /// The runtime directory's path is not absolute and canonical, holds a reserved character,
    /// or leads through a symbolic link.
    Runtime {
        /// The runtime directory.
        path: PathBuf,
    },
    /// A mount point or a directory below one could not be created or checked for a mount, or
    /// the runtime directory could not be locked.
    MountPoint {
        /// The path that could not be prepared.
        path: PathBuf,
        /// The error.
        error: io::Error,
    },
    /// Another root holds the runtime directory: one that has not been torn down, or one whose
    /// assembly is in progress, in this process or another.
    Reserved {
        /// The runtime directory.
        path: PathBuf,
    },
    /// A file system, or a bind mount of a directory, is mounted already at one of the runtime
    /// directory's mount points, such as one that a failed teardown left.
    Occupied {
        /// The mount point.
        path: PathBuf,
    },
    /// The kernel's MMIO windows, or a window's devices, could not be read.
    Lookup {
        /// The role of the device that was looked up.
        role: Role,
        /// The base address of its MMIO window.
        mmio: u64,
        /// The error.
        error: io::Error,
    },
    /// No block device appeared at an MMIO window in time.
    Missing {
        /// The role of the device that did not appear.
        role: Role,
        /// The base address of its MMIO window.
        mmio: u64,
    },
    /// A device's size is not the expected one.
    Size {
        /// The device's role.
        role: Role,
        /// The device's node.
        device: PathBuf,
        /// The expected size, in 512-byte sectors.
        expected: u64,
        /// The device's size, in 512-byte sectors.
        actual: u64,
    },
    /// A device's access mode is not its role's: the distro device must be read-only, and the
    /// scratch device writable.
    Access {
        /// The device's role.
        role: Role,
        /// The device's node.
        device: PathBuf,
        /// Whether the device is read-only.
        read_only: bool,
    },
    /// A step on a device failed.
    Device {
        /// The device's role.
        role: Role,
        /// The step that failed.
        step: DeviceStep,
        /// The device's node.
        device: PathBuf,
        /// The error.
        error: io::Error,
    },
    /// The distro device holds no EROFS superblock, so it has no UUID to check.
    NotErofs {
        /// The device's node.
        device: PathBuf,
    },
    /// The EROFS image's UUID is not the expected one.
    Uuid {
        /// The expected UUID.
        expected: Uuid,
        /// The image's UUID.
        actual: Uuid,
    },
    /// The GPT disk on the distro device failed its validation: its partition table could not be
    /// read, is invalid, or differs from the caller's expectations.
    Disk {
        /// The distro device's node.
        device: PathBuf,
        /// Why the disk failed.
        error: DeviceError,
    },
    /// The first partition of the GPT disk on the distro device is larger than the caller's
    /// bound, or could not be read.
    FirstPartition {
        /// The distro device's node.
        device: PathBuf,
        /// Why the first partition failed.
        error: GptError,
    },
    /// The kernel created no block device for a layer's partition: no node has its name.
    Partition {
        /// The partition's number.
        number: u8,
        /// The node where the partition's block device should be.
        node: PathBuf,
    },
    /// A layer's partition of the GPT disk holds no ext4 file system.
    NotExt4Layer {
        /// The partition's number.
        number: u8,
        /// The node of the partition's block device.
        device: PathBuf,
    },
    /// The ext4 file system of a layer's partition needs a journal recovery, which its read-only
    /// device can't take.
    Recovery {
        /// The partition's number.
        number: u8,
        /// The node of the partition's block device.
        device: PathBuf,
    },
    /// The scratch device holds no ext4 file system, so it has no journal feature to check.
    NotExt4 {
        /// The device's node.
        device: PathBuf,
    },
    /// The scratch's ext4 file system has a journal, which the caller forbids.
    Journal {
        /// The device's node.
        device: PathBuf,
    },
    /// The upper's `upper` or `work` directory exists and is not empty, which an assembly that
    /// requires a fresh upper refuses, as a clone's application does, whose root must start as its
    /// image alone.
    NotFresh {
        /// The directory.
        path: PathBuf,
    },
    /// The overlay's options are longer than [`MOUNT_OPTIONS_LIMIT`]: its lower directories, with
    /// the runtime directory's path, are too many or too long for one mount.
    OverlayOptions {
        /// The options' length, in bytes.
        length: usize,
    },
    /// The kernel mounted the overlay, but not as the assembly asked: read-only, or without an
    /// option of the overlay mode's set in effect. The kernel does so, without failing the mount,
    /// when it can't create the overlay's work directory, or when the upper can't hold the
    /// overlay's extended attributes.
    OverlayFallback {
        /// What the assembly asked for: `rw`, or an option of the overlay mode's set, such as
        /// `metacopy=on`.
        requested: String,
        /// What the kernel applied in its place, such as `ro` or `metacopy=off`.
        effective: String,
    },
    /// The overlay's effective options could not be read: its options in the kernel's mount
    /// table, or the default of one of them, which a parameter of the overlay module holds.
    OverlayCheck {
        /// The overlay's mount point, or the parameter's file.
        path: PathBuf,
        /// The error.
        error: io::Error,
    },
    /// A mount failed.
    Mount {
        /// The mount that failed.
        mount: Mount,
        /// The error.
        error: io::Error,
    },
    /// A live share, a child of an aggregate live share, or a mapping was refused or failed to
    /// attach at its target in the root, once the root had checked its request; nothing of it
    /// stays mounted, unless the error is a moved mapping that stays mounted. The root refuses
    /// every later share, export, and mapping, with [`MappingError::Latched`].
    Mapping {
        /// The mount that was refused or failed.
        mount: Mount,
        /// Why.
        error: MappingError,
    },
    /// A live share, or the host export, was refused before anything was mounted for it.
    Share(ShareRefusal),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bootstrap { step, error } => write!(formatter, "failed to {step}: {error}"),
            Self::Runtime { path } => write!(
                formatter,
                "the runtime directory is not a canonical absolute path without `,`, `:`, `\\`, \
                 `=`, or a symbolic link: {}",
                path.display()
            ),
            Self::MountPoint { path, error } => {
                write!(formatter, "failed to prepare {}: {error}", path.display())
            }
            Self::Reserved { path } => write!(
                formatter,
                "another root holds the runtime directory {}",
                path.display()
            ),
            Self::Occupied { path } => write!(
                formatter,
                "a file system is mounted at {} already",
                path.display()
            ),
            Self::Lookup { role, mmio, error } => write!(
                formatter,
                "failed to look up the {role} device at MMIO {mmio:#x}: {error}"
            ),
            Self::Missing { role, mmio } => write!(
                formatter,
                "the {role} device at MMIO {mmio:#x} did not appear"
            ),
            Self::Size {
                role,
                device,
                expected,
                actual,
            } => write!(
                formatter,
                "the {role} device {} has {actual} sectors, expected {expected}",
                device.display()
            ),
            Self::Access {
                role,
                device,
                read_only,
            } => {
                let (actual, expected) = if *read_only {
                    ("read-only", "writable")
                } else {
                    ("writable", "read-only")
                };
                write!(
                    formatter,
                    "the {role} device {} is {actual}, expected {expected}",
                    device.display()
                )
            }
            Self::Device {
                role,
                step,
                device,
                error,
            } => write!(
                formatter,
                "failed to {step} the {role} device {}: {error}",
                device.display()
            ),
            Self::NotErofs { device } => write!(
                formatter,
                "the distro device {} holds no EROFS image",
                device.display()
            ),
            Self::Uuid { expected, actual } => write!(
                formatter,
                "the distro layer's UUID is {actual}, expected {expected}"
            ),
            Self::Disk { device, error } => write!(
                formatter,
                "the GPT disk on the distro device {} failed its validation: {error}",
                device.display()
            ),
            Self::FirstPartition { device, error } => write!(
                formatter,
                "failed to read the first partition of the GPT disk on the distro device {}: \
                 {error}",
                device.display()
            ),
            Self::Partition { number, node } => write!(
                formatter,
                "the distro's partition {number} has no block device at {}",
                node.display()
            ),
            Self::NotExt4Layer { number, device } => write!(
                formatter,
                "the distro's partition {number}, {}, holds no ext4 file system",
                device.display()
            ),
            Self::Recovery { number, device } => write!(
                formatter,
                "the layer in partition {number}, {}, needs a journal recovery, which its \
                 read-only device can't take",
                device.display()
            ),
            Self::NotExt4 { device } => write!(
                formatter,
                "the scratch device {} holds no ext4 file system",
                device.display()
            ),
            Self::Journal { device } => write!(
                formatter,
                "the scratch device {} has an ext4 journal, which the assembly forbids",
                device.display()
            ),
            Self::NotFresh { path } => write!(
                formatter,
                "the upper's directory {} is not empty, but the upper must be fresh",
                path.display()
            ),
            Self::OverlayOptions { length } => write!(
                formatter,
                "the overlay's options take {length} bytes, more than the \
                 {MOUNT_OPTIONS_LIMIT} that a mount passes"
            ),
            Self::OverlayFallback {
                requested,
                effective,
            } => write!(
                formatter,
                "the kernel mounted the overlay with {effective} in place of {requested}"
            ),
            Self::OverlayCheck { path, error } => write!(
                formatter,
                "failed to read the overlay's effective options at {}: {error}",
                path.display()
            ),
            Self::Mount { mount, error } => write!(formatter, "failed to mount {mount}: {error}"),
            Self::Mapping { mount, error } => write!(formatter, "failed to mount {mount}: {error}"),
            Self::Share(refusal) => refusal.fmt(formatter),
        }
    }
}

impl std::error::Error for Error {}

/// Why [`assemble`] failed, and how unmounting what it had mounted failed, if it did.
#[derive(Debug)]
pub struct AssemblyError {
    /// The assembly's failure.
    error: Error,
    /// The failures to unmount what the assembly had mounted.
    cleanup: Option<TeardownError>,
}

impl AssemblyError {
    /// Returns the assembly's failure.
    pub fn error(&self) -> &Error {
        &self.error
    }

    /// Returns the failures to unmount what the assembly had mounted, if any: those mounts stay.
    pub fn cleanup(&self) -> Option<&TeardownError> {
        self.cleanup.as_ref()
    }

    /// Returns the assembly's failure and the cleanup's.
    pub fn into_parts(self) -> (Error, Option<TeardownError>) {
        (self.error, self.cleanup)
    }
}

impl fmt::Display for AssemblyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)?;
        match &self.cleanup {
            Some(cleanup) => write!(formatter, "; then {cleanup}"),
            None => Ok(()),
        }
    }
}

impl std::error::Error for AssemblyError {}

/// The syncs and unmounts of a teardown that failed, in the order in which they were attempted.
#[derive(Debug)]
pub struct TeardownError {
    /// The failures, which is never empty.
    failures: Vec<Unmount>,
}

impl TeardownError {
    /// Returns the failures, each of a mount that stays in place, in the order in which they were
    /// attempted.
    pub fn failures(&self) -> &[Unmount] {
        &self.failures
    }
}

impl fmt::Display for TeardownError {
    /// Writes each failure, separated by `; `.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, failure) in self.failures.iter().enumerate() {
            if index > 0 {
                formatter.write_str("; ")?;
            }
            failure.fmt(formatter)?;
        }
        Ok(())
    }
}

impl std::error::Error for TeardownError {}

/// An unmount that failed, which left its mount in place, or the sync of an ext4 scratch's file
/// system that failed before its unmount, which the teardown then skips.
#[derive(Debug)]
pub struct Unmount {
    /// The mount.
    mount: Mount,
    /// Its mount point.
    path: PathBuf,
    /// The step that failed.
    step: Step,
    /// The error.
    error: io::Error,
}

/// The step of a mount's teardown that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// The sync of an ext4 scratch's file system, before its unmount.
    Sync,
    /// The unmount.
    Unmount,
}

impl Unmount {
    /// Returns the mount that stays in place.
    pub fn mount(&self) -> &Mount {
        &self.mount
    }

    /// Returns its mount point.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the error of its sync or of its unmount.
    pub fn error(&self) -> &io::Error {
        &self.error
    }
}

impl fmt::Display for Unmount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let step = match self.step {
            Step::Sync => "sync",
            Step::Unmount => "unmount",
        };
        write!(formatter, "failed to {step} {}: {}", self.mount, self.error)
    }
}
