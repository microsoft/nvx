use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};

use crate::guest::init;
use crate::guest::mappings::{Kernel as MappingKernel, SystemKernel};
use crate::guest::sys::{self, Errno, FileDescriptor, Mode, OpenFlags};

/// Production paths used by the managed agent and replaceable paths used by tests.
#[derive(Clone, Debug)]
pub(crate) struct Paths {
    /// Parent of OpenVMM's aggregate host-filesystem exports.
    pub(crate) hostfs_dir: PathBuf,
    /// Kernel command line containing the announced mapping count.
    pub(crate) proc_cmdline: PathBuf,
    /// Port device used for fatal diagnostics.
    pub(crate) diagnostic_port: PathBuf,
    /// VM exit helper.
    pub(crate) exit: PathBuf,
}

impl Default for Paths {
    /// Returns the paths installed in production guest images.
    fn default() -> Self {
        Self {
            hostfs_dir: "/run/nvx/hostfs".into(),
            proc_cmdline: "/proc/cmdline".into(),
            diagnostic_port: "/dev/port".into(),
            exit: "/sbin/nvx-exit".into(),
        }
    }
}

/// Replaceable boundary for privileged and process-global operations.
pub(crate) trait System {
    /// Returns the production or test path set.
    fn paths(&self) -> &Paths;

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
    /// Starts a direct workload through init's native plain-process core.
    fn start_direct(
        &mut self,
        request: init::ExecRequest,
    ) -> Result<init::Exec, init::StartFailure> {
        init::Exec::start(request)
    }
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
}

/// Converts a path through the system layer for use by the seam and callers.
pub(crate) fn path_c_string(path: &Path) -> Result<CString, Errno> {
    sys::path_c_string(path)
}

#[cfg(test)]
pub(crate) mod tests;
