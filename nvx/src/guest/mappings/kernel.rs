//! The kernel interfaces of the mapper, which tests replace.

use std::ffi::CStr;

use crate::guest::sys::{
    self, Errno, FileDescriptor, FileStatus, Mode, MountAttributes, OpenFlags,
};

/// The kernel's operations that a mapper performs, each on descriptors or on unique mount IDs:
/// opening a path beneath a directory, reading an open file's status, telling whether a mount
/// shows its whole file system, creating a directory or a regular file, setting an open file's
/// mode, the mount API's copy, restriction, and attachment of a mount, and the unmount or
/// detachment of one.
///
/// [`SystemKernel`] performs them through `sys`. The root operations' fake kernel, which their
/// tests and the mapper's share, performs the path operations in a temporary directory, records
/// the mount operations, simulates their mounts by their IDs, and fails the operations that a
/// test chooses.
pub(crate) trait Kernel {
    /// Opens `path` beneath `directory`, as [`sys::open_beneath`] does: through no symbolic link
    /// or magic link, and crossing into the mounts below the directory only when
    /// `cross_mounts` is set.
    fn open_beneath(
        &mut self,
        directory: &FileDescriptor,
        path: &CStr,
        flags: OpenFlags,
        mode: Mode,
        cross_mounts: bool,
    ) -> Result<FileDescriptor, Errno>;

    /// Returns the kind of the file that `descriptor` opened, its mount's unique ID, and whether
    /// it is the mount's root.
    fn status(&mut self, descriptor: &FileDescriptor) -> Result<FileStatus, Errno>;

    /// Returns whether the mount whose unique ID is `mount` shows its whole file system, as
    /// [`sys::is_whole_file_system`] does: whether its root is its file system's root.
    fn whole_file_system(&mut self, mount: u64) -> Result<bool, Errno>;

    /// Creates the directory `name` in `directory` with `mode`, but for the permissions that the
    /// umask, or a default ACL of `directory`, withholds, as [`sys::create_directory_at`] does,
    /// following no symbolic link at `name`.
    fn create_directory(
        &mut self,
        directory: &FileDescriptor,
        name: &CStr,
        mode: Mode,
    ) -> Result<(), Errno>;

    /// Creates the empty regular file `name` in `directory` with exactly `mode`, whatever the
    /// umask or a default ACL of `directory`, as [`sys::create_file_at`] does, failing with
    /// `EEXIST` on any entry of that name.
    fn create_file(
        &mut self,
        directory: &FileDescriptor,
        name: &CStr,
        mode: Mode,
    ) -> Result<(), Errno>;

    /// Sets the mode of the file that `descriptor`, which may be an `O_PATH` descriptor, refers
    /// to, as [`sys::change_mode`] does.
    fn change_mode(&mut self, descriptor: &FileDescriptor, mode: Mode) -> Result<(), Errno>;

    /// Returns a descriptor of a detached copy of the mount at `descriptor`, alone, when `clone`
    /// is set, and of that mount itself otherwise.
    fn open_tree(
        &mut self,
        descriptor: &FileDescriptor,
        clone: bool,
    ) -> Result<FileDescriptor, Errno>;

    /// Sets the attributes `set` of the mount whose root `tree` refers to, and clears `clear`.
    fn set_attributes(
        &mut self,
        tree: &FileDescriptor,
        set: MountAttributes,
        clear: MountAttributes,
    ) -> Result<(), Errno>;

    /// Moves the mount whose root `tree` refers to onto the file that `target` refers to.
    fn move_mount(&mut self, tree: &FileDescriptor, target: &FileDescriptor) -> Result<(), Errno>;

    /// Unmounts the mount on the entry `name` of `directory`, without following a symbolic link
    /// at `name`, and without detaching a busy one.
    fn unmount_beneath(&mut self, directory: &FileDescriptor, name: &CStr) -> Result<(), Errno>;

    /// Detaches the mount whose root `tree` refers to from where it is attached, even if it is
    /// busy, as a lazy unmount does.
    fn detach(&mut self, tree: &FileDescriptor) -> Result<(), Errno>;
}

/// The kernel itself.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SystemKernel;

impl Kernel for SystemKernel {
    fn open_beneath(
        &mut self,
        directory: &FileDescriptor,
        path: &CStr,
        flags: OpenFlags,
        mode: Mode,
        cross_mounts: bool,
    ) -> Result<FileDescriptor, Errno> {
        sys::open_beneath(directory, path, flags, mode, cross_mounts)
    }

    fn status(&mut self, descriptor: &FileDescriptor) -> Result<FileStatus, Errno> {
        sys::file_status(descriptor)
    }

    fn whole_file_system(&mut self, mount: u64) -> Result<bool, Errno> {
        sys::is_whole_file_system(mount)
    }

    fn create_directory(
        &mut self,
        directory: &FileDescriptor,
        name: &CStr,
        mode: Mode,
    ) -> Result<(), Errno> {
        sys::create_directory_at(directory, name, mode)
    }

    fn create_file(
        &mut self,
        directory: &FileDescriptor,
        name: &CStr,
        mode: Mode,
    ) -> Result<(), Errno> {
        sys::create_file_at(directory, name, mode)
    }

    fn change_mode(&mut self, descriptor: &FileDescriptor, mode: Mode) -> Result<(), Errno> {
        sys::change_mode(descriptor, mode)
    }

    fn open_tree(
        &mut self,
        descriptor: &FileDescriptor,
        clone: bool,
    ) -> Result<FileDescriptor, Errno> {
        sys::open_tree(descriptor, clone)
    }

    fn set_attributes(
        &mut self,
        tree: &FileDescriptor,
        set: MountAttributes,
        clear: MountAttributes,
    ) -> Result<(), Errno> {
        sys::set_mount_attributes(tree, set, clear)
    }

    fn move_mount(&mut self, tree: &FileDescriptor, target: &FileDescriptor) -> Result<(), Errno> {
        sys::move_mount(tree, target)
    }

    fn unmount_beneath(&mut self, directory: &FileDescriptor, name: &CStr) -> Result<(), Errno> {
        sys::unmount_beneath(directory, name)
    }

    fn detach(&mut self, tree: &FileDescriptor) -> Result<(), Errno> {
        sys::detach_mount(tree)
    }
}
