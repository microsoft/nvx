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
pub(crate) use crate::guest::root::attach_calls;

/// One recorded `exec`: the path, the arguments, and the environment if it had one.
#[cfg(test)]
pub(crate) type RecordedExec = (Vec<u8>, Vec<Vec<u8>>, Option<Vec<Vec<u8>>>);

#[cfg(test)]
pub(crate) struct TestSystem {
    pub(crate) paths: Paths,
    pub(crate) mounts: Vec<(Option<Vec<u8>>, Vec<u8>, MountFlags)>,
    pub(crate) fail_mount_call: Option<usize>,
    /// The root operations' fake kernel, through which the host mapping table's mapper works in
    /// the test's temporary directories, and which records its mounts.
    pub(crate) kernel: crate::guest::root::FakeKernel,
    pub(crate) diagnostics: Vec<u8>,
    pub(crate) fail_environment: Option<Vec<u8>>,
    pub(crate) execs: Vec<RecordedExec>,
    pub(crate) spawn_calls: usize,
    /// Spawns each workload's launch command when set; `spawn` otherwise fails with `ENOSYS`.
    pub(crate) spawner: Option<fn(Command) -> Result<Spawned, Errno>>,
    pub(crate) tty_configured: usize,
    pub(crate) tty_drained: usize,
    /// Owned descriptors awaiting a simulated helper-entry transfer.
    pub(crate) inherited_descriptors: Vec<(Vec<u8>, FileDescriptor)>,
    /// The sandbox executions that `prepare_entry` prepared, each simulated, without
    /// privileges: see [`Prepared::simulated`].
    pub(crate) entries_prepared: usize,
    /// The entry stage that fails, with its error, in each simulated sandbox execution.
    pub(crate) entry_failure: Option<(crate::guest::spawn::LaunchStage, Errno)>,
    /// Makes `prepare_entry` refuse each sandbox execution, as when its account is missing.
    pub(crate) refuse_entry: bool,
    /// The calls of `bind_runtime`, which binds nothing.
    pub(crate) runtime_binds: usize,
    /// The error with which `bind_runtime` fails, if any.
    pub(crate) fail_runtime_bind: Option<Errno>,
}

#[cfg(test)]
impl TestSystem {
    pub(crate) fn new(paths: Paths) -> Self {
        Self {
            paths,
            mounts: Vec::new(),
            fail_mount_call: None,
            kernel: crate::guest::root::FakeKernel::default(),
            diagnostics: Vec::new(),
            fail_environment: None,
            execs: Vec::new(),
            spawn_calls: 0,
            spawner: None,
            tty_configured: 0,
            tty_drained: 0,
            inherited_descriptors: Vec::new(),
            entries_prepared: 0,
            entry_failure: None,
            refuse_entry: false,
            runtime_binds: 0,
            fail_runtime_bind: None,
        }
    }

    /// Returns a system whose cgroup root, in a new temporary directory that the caller keeps,
    /// holds empty membership and controller files and an `nvx-exec` cgroup without tasks.
    pub(crate) fn with_cgroup() -> (tempfile::TempDir, Self) {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("cgroup");
        let execution = root.join("nvx-exec");
        std::fs::create_dir_all(&execution).unwrap();
        std::fs::write(root.join("cgroup.procs"), b"").unwrap();
        std::fs::write(root.join("cgroup.subtree_control"), b"").unwrap();
        std::fs::write(execution.join("pids.current"), b"0\n").unwrap();
        std::fs::write(execution.join("cgroup.kill"), b"").unwrap();
        std::fs::write(execution.join("cgroup.procs"), b"").unwrap();
        let paths = Paths {
            cgroup_root: root,
            ..Paths::default()
        };
        (temporary, Self::new(paths))
    }

    /// Returns a system for the sandbox agent, in a new temporary directory that the caller
    /// keeps: its cgroup root holds the agent's `agent` cgroup and its workloads' `container`
    /// cgroup, with an empty membership file, and its machine ID's path is free.
    pub(crate) fn with_container_cgroup() -> (tempfile::TempDir, Self) {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("cgroup");
        std::fs::create_dir_all(root.join("agent")).unwrap();
        std::fs::create_dir_all(root.join("container")).unwrap();
        std::fs::write(root.join("container/cgroup.procs"), b"").unwrap();
        let paths = Paths {
            cgroup_root: root,
            machine_id: temporary.path().join("workload-machine-id"),
            time_status: temporary.path().join("time"),
            ..Paths::default()
        };
        (temporary, Self::new(paths))
    }

    /// Returns a system whose host-filesystem export, in a new temporary directory that the
    /// caller keeps, holds child `0`, with the directory `src` and the file `notes.txt`, and the
    /// empty child `1`.
    pub(crate) fn with_host_export() -> (tempfile::TempDir, Self) {
        let temporary = tempfile::tempdir().unwrap();
        let hostfs = temporary.path().join("hostfs");
        std::fs::create_dir_all(hostfs.join("root/0/src")).unwrap();
        std::fs::write(hostfs.join("root/0/notes.txt"), b"notes").unwrap();
        std::fs::create_dir(hostfs.join("root/1")).unwrap();
        let paths = Paths {
            hostfs_dir: hostfs,
            ..Paths::default()
        };
        (temporary, Self::new(paths))
    }

    /// Registers one owned descriptor and returns its simulated inherited argument.
    pub(crate) fn transfer_inherited_descriptor(&mut self, descriptor: FileDescriptor) -> CString {
        let argument = descriptor.argument();
        self.inherited_descriptors
            .push((argument.as_bytes().to_vec(), descriptor));
        argument
    }
}

#[cfg(test)]
impl System for TestSystem {
    fn paths(&self) -> &Paths {
        &self.paths
    }

    fn mount(
        &mut self,
        source: Option<&CStr>,
        target: &CStr,
        _filesystem: Option<&CStr>,
        flags: MountFlags,
    ) -> Result<(), Errno> {
        self.mounts.push((
            source.map(|source| source.to_bytes().to_vec()),
            target.to_bytes().to_vec(),
            flags,
        ));
        if self.fail_mount_call == Some(self.mounts.len()) {
            Err(Errno::ENOENT)
        } else {
            Ok(())
        }
    }

    fn kernel(&mut self) -> &mut dyn MappingKernel {
        &mut self.kernel
    }

    fn chown(&mut self, _path: &CStr, _uid: u32, _gid: u32) -> Result<(), Errno> {
        Ok(())
    }

    fn chmod(&mut self, _path: &CStr, _mode: Mode) -> Result<(), Errno> {
        Ok(())
    }

    fn configure_tty(&mut self, _descriptor: &FileDescriptor) -> Result<(), Errno> {
        self.tty_configured += 1;
        Ok(())
    }

    fn drain_tty(&mut self, _descriptor: &FileDescriptor) -> Result<(), Errno> {
        self.tty_drained += 1;
        Ok(())
    }

    fn write_diagnostic(&mut self, message: &[u8]) -> Result<(), Errno> {
        self.diagnostics.extend_from_slice(message);
        Ok(())
    }

    fn take_inherited_descriptor(&mut self, text: &CStr) -> Result<FileDescriptor, Errno> {
        let index = self
            .inherited_descriptors
            .iter()
            .position(|(argument, _)| argument == text.to_bytes())
            .ok_or(Errno::EBADF)?;
        Ok(self.inherited_descriptors.swap_remove(index).1)
    }

    fn validate_environment(&mut self, name: &CStr, _value: &CStr) -> Result<(), Errno> {
        if self
            .fail_environment
            .as_deref()
            .is_some_and(|failure| failure == name.to_bytes())
        {
            Err(Errno::ENOMEM)
        } else {
            Ok(())
        }
    }

    fn exec(
        &mut self,
        path: &CStr,
        arguments: &[CString],
        environment: Option<&[CString]>,
    ) -> Result<(), Errno> {
        let bytes = |strings: &[CString]| {
            strings
                .iter()
                .map(|string| string.as_bytes().to_vec())
                .collect::<Vec<_>>()
        };
        self.execs.push((
            path.to_bytes().to_vec(),
            bytes(arguments),
            environment.map(bytes),
        ));
        Ok(())
    }

    fn spawn(&mut self, command: Command) -> Result<Spawned, Errno> {
        self.spawn_calls += 1;
        match self.spawner {
            Some(spawner) => spawner(command),
            None => Err(Errno::ENOSYS),
        }
    }

    fn prepare_entry(&mut self, _entry: &Entry) -> Result<Prepared, entry::Error> {
        self.entries_prepared += 1;
        if self.refuse_entry {
            return Err(entry::Error::Account(entry::AccountError::Missing));
        }
        Ok(Prepared::simulated(self.entry_failure))
    }

    fn bind_runtime(&mut self, _entry: &Entry) -> Result<(), Errno> {
        self.runtime_binds += 1;
        self.fail_runtime_bind.map_or(Ok(()), Err)
    }
}

/// Spawns a launch command as the privileged agent does, without privileges: the child's
/// credential calls are replaced and succeed, and it enters the working directory with the test's
/// own credentials.
#[cfg(test)]
pub(crate) fn spawn_as_privileged(command: Command) -> Result<Spawned, Errno> {
    command
        .simulate_credentials(None)
        .spawn()
        .map_err(|error| sys::errno_of(&error))
}

/// Spawns a launch command whose replaced credential calls fail with `EPERM`, as `setgroups`
/// does without privileges.
#[cfg(test)]
pub(crate) fn spawn_with_failing_credentials(command: Command) -> Result<Spawned, Errno> {
    command
        .simulate_credentials(Some(Errno::EPERM))
        .spawn()
        .map_err(|error| sys::errno_of(&error))
}

/// Writes an executable stand-in for `setpriv` in `directory` and returns its path.
///
/// The stand-in runs the shell command `action`, then executes the workload that follows the
/// helper's `--` separator, without changing credentials and without the helper. `action` runs
/// after the direct child has entered its working directory and before the workload runs.
#[cfg(test)]
pub(crate) fn setpriv_shim(directory: &Path, action: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let path = directory.join("setpriv");
    let script = format!(
        "#!/bin/sh\nset -e\n{action}\nwhile [ \"$#\" -gt 0 ]; do\n    argument=$1\n    shift\n    \
         if [ \"$argument\" = -- ]; then\n        break\n    fi\ndone\nexec \"$@\"\n"
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}
