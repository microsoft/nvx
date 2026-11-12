use super::*;

pub(crate) use crate::guest::root::tests::attach_calls;

/// One recorded `exec`: the path, the arguments, and the environment if it had one.
pub(crate) type RecordedExec = (Vec<u8>, Vec<Vec<u8>>, Option<Vec<Vec<u8>>>);

pub(crate) struct TestSystem {
    pub(crate) paths: Paths,
    /// The root operations' fake kernel, through which the host mapping table's mapper works in
    /// the test's temporary directories, and which records its mounts.
    pub(crate) kernel: crate::guest::root::tests::FakeKernel,
    pub(crate) diagnostics: Vec<u8>,
    pub(crate) fail_environment: Option<Vec<u8>>,
    pub(crate) execs: Vec<RecordedExec>,
    pub(crate) spawn_calls: usize,
    /// Spawns each workload's launch command when set; `spawn` otherwise fails with `ENOSYS`.
    pub(crate) spawner: Option<fn(Command) -> Result<Spawned, Errno>>,
    pub(crate) direct_starter:
        Option<fn(init::ExecRequest) -> Result<init::Exec, init::StartFailure>>,
    pub(crate) tty_configured: usize,
    pub(crate) tty_drained: usize,
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

impl TestSystem {
    pub(crate) fn new(paths: Paths) -> Self {
        Self {
            paths,
            kernel: crate::guest::root::tests::FakeKernel::default(),
            diagnostics: Vec::new(),
            fail_environment: None,
            execs: Vec::new(),
            spawn_calls: 0,
            spawner: None,
            direct_starter: None,
            tty_configured: 0,
            tty_drained: 0,
            entries_prepared: 0,
            entry_failure: None,
            refuse_entry: false,
            runtime_binds: 0,
            fail_runtime_bind: None,
        }
    }

    /// Returns a direct agent with no cgroup hierarchy in its temporary directory.
    #[cfg(feature = "cli")]
    pub(crate) fn with_direct() -> (tempfile::TempDir, Self) {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("cgroup");
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
}

impl System for TestSystem {
    fn paths(&self) -> &Paths {
        &self.paths
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

    fn start_direct(
        &mut self,
        request: init::ExecRequest,
    ) -> Result<init::Exec, init::StartFailure> {
        self.spawn_calls += 1;
        match self.direct_starter {
            Some(start) => start(request),
            None => Err(Errno::ENOSYS.into()),
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

/// Starts a native direct workload as the privileged agent does, without privileges: the child's
/// credential calls are replaced and succeed, and it enters the working directory with the test's
/// own credentials.
#[cfg(feature = "cli")]
pub(crate) fn start_as_privileged(
    request: init::ExecRequest,
) -> Result<init::Exec, init::StartFailure> {
    init::Exec::start_simulated(request, None)
}

/// Starts a native direct workload whose simulated identity stage fails with `EPERM`.
#[cfg(feature = "cli")]
pub(crate) fn start_with_failing_credentials(
    request: init::ExecRequest,
) -> Result<init::Exec, init::StartFailure> {
    init::Exec::start_simulated(request, Some(Errno::EPERM))
}
