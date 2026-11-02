use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};

use crate::guest::entry::{self, Entry, Prepared};
use crate::guest::mappings::{Kernel as MappingKernel, SystemKernel};
use crate::guest::spawn::{Command, Spawned};
use crate::guest::sys::{self, Errno, FileDescriptor, Mode, MountFlags, OpenFlags};

/// Production paths used by the managed agent and replaceable paths used by tests.
#[derive(Clone, Debug)]
pub(crate) struct Paths {
    /// Unified cgroup-v2 root, which holds the agent's cgroup and its workloads' in each mode.
    pub(crate) cgroup_root: PathBuf,
    /// Parent of OpenVMM's aggregate host-filesystem exports.
    pub(crate) hostfs_dir: PathBuf,
    /// Kernel command line containing the announced mapping count.
    pub(crate) proc_cmdline: PathBuf,
    /// Port device used for fatal diagnostics.
    pub(crate) diagnostic_port: PathBuf,
    /// Null device used for child standard input.
    pub(crate) null_device: PathBuf,
    /// The machine ID that the sandbox agent generates once, and binds for its executions.
    pub(crate) machine_id: PathBuf,
    /// The guest's time status, which the sandbox agent binds for its executions.
    pub(crate) time_status: PathBuf,
    /// Managed-agent helper executable.
    pub(crate) helper: PathBuf,
    /// VM exit helper.
    pub(crate) exit: PathBuf,
    /// Privilege-dropping launcher used in direct mode.
    pub(crate) setpriv: PathBuf,
}

impl Default for Paths {
    /// Returns the paths installed in production guest images.
    fn default() -> Self {
        Self {
            cgroup_root: "/sys/fs/cgroup".into(),
            hostfs_dir: "/run/nvx/hostfs".into(),
            proc_cmdline: "/proc/cmdline".into(),
            diagnostic_port: "/dev/port".into(),
            null_device: "/dev/null".into(),
            machine_id: "/run/nvx/workload-machine-id".into(),
            time_status: "/run/nvx/time".into(),
            helper: "/sbin/nvx-managed-agent".into(),
            exit: "/sbin/nvx-exit".into(),
            setpriv: "/bin/setpriv".into(),
        }
    }
}

/// Replaceable boundary for privileged and process-global operations.
pub(crate) trait System {
    /// Returns the production or test path set.
    fn paths(&self) -> &Paths;

    /// Performs one mount or bind-remount operation.
    fn mount(
        &mut self,
        source: Option<&CStr>,
        target: &CStr,
        filesystem: Option<&CStr>,
        flags: MountFlags,
    ) -> Result<(), Errno>;

    /// Returns the kernel through which the host mapping table's mapper opens, creates, and
    /// mounts.
    fn kernel(&mut self) -> &mut dyn MappingKernel;

    /// Changes path ownership.
    fn chown(&mut self, path: &CStr, uid: u32, gid: u32) -> Result<(), Errno>;
    /// Changes path mode bits.
    fn chmod(&mut self, path: &CStr, mode: Mode) -> Result<(), Errno>;
    /// Configures the control terminal.
    fn configure_tty(&mut self, descriptor: &FileDescriptor) -> Result<(), Errno>;
    /// Drains pending control-terminal output.
    fn drain_tty(&mut self, descriptor: &FileDescriptor) -> Result<(), Errno>;
    /// Writes one diagnostic line through port 0xE9.
    fn write_diagnostic(&mut self, message: &[u8]) -> Result<(), Errno>;
    /// Returns an owned duplicate of the helper's inherited configuration descriptor, which then
    /// closes on `exec`.
    fn take_inherited_descriptor(&mut self, text: &CStr) -> Result<FileDescriptor, Errno>;
    /// Validates one variable that an environment receives, without setting it.
    ///
    /// Production accepts every variable: building an environment in memory cannot fail where C's
    /// environment functions could. Tests reject one to cover the C failure paths.
    fn validate_environment(&mut self, name: &CStr, value: &CStr) -> Result<(), Errno>;
    /// Replaces the current process image, with `environment` or the current one.
    fn exec(
        &mut self,
        path: &CStr,
        arguments: &[CString],
        environment: Option<&[CString]>,
    ) -> Result<(), Errno>;
    /// Spawns one workload's launch command.
    fn spawn(&mut self, command: Command) -> Result<Spawned, Errno>;
    /// Prepares one execution of the sandbox agent's `entry`.
    fn prepare_entry(&mut self, entry: &Entry) -> Result<Prepared, entry::Error>;
    /// Moves the sandbox agent into a mount namespace of its own, and binds `entry`'s runtime
    /// files into its root there, once, for every execution, whose namespace starts as a copy of
    /// the agent's. The binds end with the agent, before the init agent unmounts the root.
    fn bind_runtime(&mut self, entry: &Entry) -> Result<(), Errno>;
}

/// Production implementation backed exclusively by safe `guest::sys` APIs.
pub(crate) struct RealSystem {
    /// Production path set.
    paths: Paths,
    /// The kernel itself, for the host mapping table's mapper.
    kernel: SystemKernel,
}

impl RealSystem {
    /// Creates a production system seam with `paths`.
    pub(crate) fn new(paths: Paths) -> Self {
        Self {
            paths,
            kernel: SystemKernel,
        }
    }
}

impl System for RealSystem {
    fn paths(&self) -> &Paths {
        &self.paths
    }

    fn mount(
        &mut self,
        source: Option<&CStr>,
        target: &CStr,
        filesystem: Option<&CStr>,
        flags: MountFlags,
    ) -> Result<(), Errno> {
        sys::mount(source, target, filesystem, flags, None)
    }

    fn kernel(&mut self) -> &mut dyn MappingKernel {
        &mut self.kernel
    }

    fn chown(&mut self, path: &CStr, uid: u32, gid: u32) -> Result<(), Errno> {
        sys::chown(path, uid, gid)
    }

    fn chmod(&mut self, path: &CStr, mode: Mode) -> Result<(), Errno> {
        sys::chmod(path, mode)
    }

    fn configure_tty(&mut self, descriptor: &FileDescriptor) -> Result<(), Errno> {
        sys::configure_raw(descriptor)
    }

    fn drain_tty(&mut self, descriptor: &FileDescriptor) -> Result<(), Errno> {
        sys::drain(descriptor)
    }

    fn write_diagnostic(&mut self, message: &[u8]) -> Result<(), Errno> {
        let path = path_c_string(&self.paths.diagnostic_port)?;
        let descriptor = sys::open(
            &path,
            OpenFlags::O_WRONLY | OpenFlags::O_CLOEXEC,
            Mode::empty(),
        )?;
        sys::write_port_diagnostic(&descriptor, message)
    }

    fn take_inherited_descriptor(&mut self, text: &CStr) -> Result<FileDescriptor, Errno> {
        sys::take_inherited_descriptor(text)
    }

    fn validate_environment(&mut self, _name: &CStr, _value: &CStr) -> Result<(), Errno> {
        Ok(())
    }

    fn exec(
        &mut self,
        path: &CStr,
        arguments: &[CString],
        environment: Option<&[CString]>,
    ) -> Result<(), Errno> {
        Err(sys::exec(path, arguments, environment))
    }

    fn spawn(&mut self, command: Command) -> Result<Spawned, Errno> {
        command.spawn().map_err(|error| sys::errno_of(&error))
    }

    fn prepare_entry(&mut self, entry: &Entry) -> Result<Prepared, entry::Error> {
        entry.prepare()
    }

    fn bind_runtime(&mut self, entry: &Entry) -> Result<(), Errno> {
        sys::enter_private_mount_namespace()?;
        // The binds end with the agent's mount namespace, before the init agent unmounts the
        // root, so the agent leaves them mounted.
        entry
            .bind_runtime()
            .map(drop)
            .map_err(|error| runtime_errno(&error))
    }
}

/// Returns the errno of a failure of [`Entry::bind_runtime`], that of the bind that failed.
fn runtime_errno(error: &entry::Error) -> Errno {
    match error {
        entry::Error::Root(error) => sys::errno_of(error),
        entry::Error::MachineId(error)
        | entry::Error::TimeStatus(error)
        | entry::Error::Resolver(error) => Errno::from_raw(error.raw_os_error()),
        entry::Error::Rollback { error, .. } => runtime_errno(error),
        _ => Errno::EINVAL,
    }
}

/// Converts a path through the system layer for use by the seam and callers.
pub(crate) fn path_c_string(path: &Path) -> Result<CString, Errno> {
    sys::path_c_string(path)
}

#[cfg(test)]
pub(crate) mod tests;
