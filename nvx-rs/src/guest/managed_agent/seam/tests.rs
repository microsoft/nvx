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
    pub(crate) direct_starter:
        Option<fn(init::ExecRequest) -> Result<init::Exec, init::StartFailure>>,
    pub(crate) tty_configured: usize,
    pub(crate) tty_drained: usize,
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
            direct_starter: None,
            tty_configured: 0,
            tty_drained: 0,
        }
    }

    /// Returns a direct agent's system, and a new temporary directory that the caller keeps.
    #[cfg(feature = "cli")]
    pub(crate) fn with_direct() -> (tempfile::TempDir, Self) {
        (tempfile::tempdir().unwrap(), Self::new(Paths::default()))
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
