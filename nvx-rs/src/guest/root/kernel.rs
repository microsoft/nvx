//! The kernel interfaces of the root operations, which tests replace.
//!
//! The operations read the kernel's text files and walk directories through the standard
//! library, at the paths of a [`Paths`], so that a test relocates them to a temporary directory.
//! What a test cannot do in a temporary directory, such as a mount, an unmount, a flush of a
//! block device's buffers, a sync of a mounted file system, a reread of a disk's partition table,
//! or a read of a GPT disk's partition table at the geometry that its block device reports, and
//! the writes to the kernel's control files, the check for a mount point, and the options of a
//! mount, whose effects and answers a test simulates, go through a [`Kernel`], which also performs
//! the mapper's operations for the root's live shares and mappings.

use std::ffi::CStr;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::gpt::{Disk, Expected};
use crate::guest::gpt::{self, DeviceError};
use crate::guest::mappings::Kernel as MappingKernel;
use crate::guest::parse::parse_decimal;
use crate::guest::sys::{self, Errno, Mode, MountFlags, OpenFlags};

pub(crate) use crate::guest::mappings::SystemKernel;

/// The kernel's table of the calling thread's mount namespace.
const MOUNT_TABLE: &str = "/proc/thread-self/mountinfo";

/// The kernel's operations that the root operations perform, other than reads: mounts and
/// unmounts, flushes of a block device's buffers, syncs of a mounted file system, rereads of a
/// disk's partition table, writes of a control file, and the sleeps between two polls for a block
/// device; the check for a mount point and the options of a mount, which depend on them; since
/// only a block device reports its geometry, the read of a GPT disk's partition table; and the
/// mapper's operations, through which the root mounts its live shares and mappings.
///
/// [`SystemKernel`] performs them. The tests' fake records the operations, simulates the effect
/// of a cgroup's membership file and its own mounts, reads a GPT disk's table from a regular
/// file, and fails the operations that a test chooses.
pub(super) trait Kernel: MappingKernel + Send + Sync {
    /// Mounts `source` at `target`, as `mount(2)` does, with the file system's own options
    /// `data`.
    fn mount(
        &mut self,
        source: Option<&CStr>,
        target: &CStr,
        filesystem: Option<&CStr>,
        flags: MountFlags,
        data: Option<&CStr>,
    ) -> Result<(), Errno>;

    /// Unmounts the file system mounted at `target`, without detaching a busy one.
    fn unmount(&mut self, target: &CStr) -> Result<(), Errno>;

    /// Writes the dirty buffers of the block device `device`, then drops its cached ones.
    fn flush(&mut self, device: &CStr) -> Result<(), Errno>;

    /// Writes the dirty data and metadata of the file system mounted at the mount point `target`
    /// to its storage, as `syncfs(2)` does.
    fn sync_file_system(&mut self, target: &CStr) -> Result<(), Errno>;

    /// Asks the kernel to reread the partition table of the disk whose whole-disk block device is
    /// `device`, as [`sys::reread_partition_table`] does.
    #[cfg(any(target_arch = "x86_64", test))]
    fn reread_partitions(&mut self, device: &CStr) -> Result<(), Errno>;

    /// Reads and checks the partition table of the GPT disk that `file` opens, the whole-disk
    /// block device whose node is `device`, with the caller's `expected` values, at the device's
    /// logical block size and size, as [`gpt::read_disk`] does.
    fn read_disk(
        &mut self,
        device: &Path,
        file: &File,
        expected: &Expected,
    ) -> Result<Disk, DeviceError>;

    /// Writes `value` whole to the existing control file `path`, such as a cgroup's or a
    /// sysctl's, and reports a failure to close it.
    fn write(&mut self, path: &CStr, value: &[u8]) -> Result<(), Errno>;

    /// Returns whether the directory `path` is a mount point: whether it is on another mount
    /// than its parent directory, as it is where a file system is mounted, or where a directory
    /// of the parent's own file system is bound.
    fn is_mount_point(&mut self, path: &Path) -> io::Result<bool>;

    /// Returns the options of the file system mounted at the mount point `path`, as the kernel's
    /// mount table shows them: `ro` or `rw`, then those of the file system's own options that
    /// differ from their defaults.
    fn mount_options(&mut self, path: &Path) -> io::Result<String>;

    /// Sleeps for `duration`.
    fn sleep(&mut self, duration: Duration);
}

impl Kernel for SystemKernel {
    fn mount(
        &mut self,
        source: Option<&CStr>,
        target: &CStr,
        filesystem: Option<&CStr>,
        flags: MountFlags,
        data: Option<&CStr>,
    ) -> Result<(), Errno> {
        sys::mount(source, target, filesystem, flags, data)
    }

    fn unmount(&mut self, target: &CStr) -> Result<(), Errno> {
        sys::unmount(target)
    }

    fn flush(&mut self, device: &CStr) -> Result<(), Errno> {
        let descriptor = sys::open(
            device,
            OpenFlags::O_RDONLY | OpenFlags::O_CLOEXEC,
            Mode::empty(),
        )?;
        sys::flush_block_buffers(&descriptor)?;
        descriptor.close()
    }

    fn sync_file_system(&mut self, target: &CStr) -> Result<(), Errno> {
        let descriptor = sys::open(
            target,
            OpenFlags::O_RDONLY | OpenFlags::O_DIRECTORY | OpenFlags::O_CLOEXEC,
            Mode::empty(),
        )?;
        descriptor.sync_file_system()?;
        descriptor.close()
    }

    #[cfg(any(target_arch = "x86_64", test))]
    fn reread_partitions(&mut self, device: &CStr) -> Result<(), Errno> {
        let descriptor = sys::open(
            device,
            OpenFlags::O_RDONLY | OpenFlags::O_CLOEXEC,
            Mode::empty(),
        )?;
        sys::reread_partition_table(&descriptor)?;
        descriptor.close()
    }

    fn read_disk(
        &mut self,
        _device: &Path,
        file: &File,
        expected: &Expected,
    ) -> Result<Disk, DeviceError> {
        gpt::read_disk(file, expected)
    }

    fn write(&mut self, path: &CStr, value: &[u8]) -> Result<(), Errno> {
        let descriptor = sys::open(
            path,
            OpenFlags::O_WRONLY | OpenFlags::O_CLOEXEC,
            Mode::empty(),
        )?;
        descriptor.write_all(value)?;
        descriptor.close()
    }

    fn is_mount_point(&mut self, path: &Path) -> io::Result<bool> {
        let parent = path.parent().ok_or(Errno::EINVAL)?;
        let mount = sys::mount_id(&sys::path_c_string(path)?)?;
        Ok(mount != sys::mount_id(&sys::path_c_string(parent)?)?)
    }

    fn mount_options(&mut self, path: &Path) -> io::Result<String> {
        let mount = sys::mount_id(&sys::path_c_string(path)?)?;
        let table = fs::read(MOUNT_TABLE)?;
        let options = super_options(&table, mount).ok_or(Errno::ENOENT)?;
        Ok(String::from_utf8_lossy(options).into_owned())
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Returns the options of the file system of the mount whose ID is `mount`, the last field of its
/// line in the mount table `table`, or `None` if the table has no such line.
///
/// A line's fields are separated by spaces, which the kernel escapes within a field: the mount's
/// ID first, and its file system's type, source, and options after a field `-`.
pub(super) fn super_options(table: &[u8], mount: u64) -> Option<&[u8]> {
    table.split(|&byte| byte == b'\n').find_map(|line| {
        let mut fields = line.split(|&byte| byte == b' ');
        if parse_decimal(fields.next()?)? != mount {
            return None;
        }
        fields.skip_while(|field| *field != b"-").nth(3)
    })
}

/// The directories of the kernel's interfaces that the root operations read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Paths {
    /// The mount point of procfs.
    pub(crate) proc: PathBuf,
    /// The mount point of sysfs.
    pub(crate) sys: PathBuf,
    /// The directory of the device nodes.
    pub(crate) dev: PathBuf,
}

impl Default for Paths {
    /// Returns the paths at which the guest mounts the kernel's interfaces.
    fn default() -> Self {
        Self {
            proc: "/proc".into(),
            sys: "/sys".into(),
            dev: "/dev".into(),
        }
    }
}

/// The tests' kernel, which records each operation and performs no mount, unmount, flush, sync,
/// or reread.
#[cfg(test)]
pub(super) mod tests;
