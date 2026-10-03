use std::fmt;
use std::io;
use std::sync::Arc;

use crate::backend::{Backend, ExecControl, ExecIo};
use crate::capabilities::Capabilities;
use crate::error::{Error, Result};
use crate::exec::Execution;
use crate::id::SandboxId;
use crate::input::InputSource;
use crate::model::{
    DeprovisionResult, ExecRequest, ProvisionRequest, ProvisionResult, StartResult, StdinMode,
    StopResult,
};
use crate::stream;
use crate::validate;

/// Entry point for the ACI Edge Sandboxes lifecycle.
///
/// `AciEdgeSandbox` validates each request, then delegates it to a [`Backend`]. It is cheap to clone, and
/// clones share the backend.
#[derive(Clone)]
pub struct AciEdgeSandbox {
    backend: Arc<dyn Backend>,
}

impl AciEdgeSandbox {
    /// Creates a client for `backend`.
    pub fn new(backend: impl Backend + 'static) -> Self {
        Self {
            backend: Arc::new(backend),
        }
    }

    /// Creates a client for a shared backend.
    pub fn from_shared(backend: Arc<dyn Backend>) -> Self {
        Self { backend }
    }

    /// Creates a client for the default backend, which drives the `openvmm` binary directly.
    #[cfg(feature = "openvmm")]
    pub fn openvmm(config: crate::openvmm::OpenVmmConfig) -> Result<Self> {
        Ok(Self::new(crate::openvmm::OpenVmmBackend::new(config)?))
    }

    /// Returns the backend.
    pub fn backend(&self) -> &dyn Backend {
        self.backend.as_ref()
    }

    /// Returns the features the backend honors.
    pub fn capabilities(&self) -> Capabilities {
        self.backend.capabilities()
    }

    /// Checks that the backend's runtime dependencies are present.
    pub fn probe(&self) -> Result<()> {
        self.backend.probe()
    }

    /// Allocates a sandbox without starting it.
    pub fn provision(&self, request: &ProvisionRequest) -> Result<ProvisionResult> {
        self.validate_provision(request)?;
        self.backend.provision(request)
    }

    /// Checks a provision request the way [`AciEdgeSandbox::provision`] does, without running anything.
    ///
    /// Reports structural problems first, then unsupported features and deterministic backend
    /// policies. Checks that
    /// need the host, such as file existence, happen only in
    /// [`AciEdgeSandbox::provision`]. Callers with a separate validation phase or dry run use this.
    pub fn validate_provision(&self, request: &ProvisionRequest) -> Result<()> {
        validate::provision_structure(request)?;
        validate::provision_capabilities(request, &self.backend.capabilities())?;
        self.backend.validate_provision(request)
    }

    /// Moves a provisioned sandbox to the running state.
    pub fn start(&self, sandbox_id: &SandboxId) -> Result<StartResult> {
        self.backend.start(sandbox_id)
    }

    /// Starts a workload in a running sandbox and returns its live streams.
    pub fn exec(&self, sandbox_id: &SandboxId, request: &ExecRequest) -> Result<Execution> {
        self.validate_exec(request)?;
        let (stdout_sink, stdout_queue) = stream::queue();
        let (stderr_sink, stderr_queue) = stream::queue();
        let (stdout, stdout_pipe) = io::pipe().map_err(pipe_error)?;
        let (stderr, stderr_pipe) = io::pipe().map_err(pipe_error)?;
        let (backend_stdin, caller_stdin) = match request.stdin {
            StdinMode::Null => (None, None),
            StdinMode::Piped => {
                let (reader, writer) = io::pipe().map_err(pipe_error)?;
                (
                    Some(InputSource::from_pipe(reader).map_err(pipe_error)?),
                    Some(writer),
                )
            }
        };
        // Forwarders start first so a failed exec simply closes the queues behind them.
        stream::forward_to_pipe(stdout_queue, stdout_pipe).map_err(thread_error)?;
        stream::forward_to_pipe(stderr_queue, stderr_pipe).map_err(thread_error)?;
        let io = ExecIo {
            stdout: Box::new(stdout_sink),
            stderr: Box::new(stderr_sink),
            stdin: backend_stdin,
        };
        let control = self.backend.exec(sandbox_id, request, io)?;
        Ok(Execution::new(
            stdout,
            stderr,
            caller_stdin,
            Arc::from(control),
        ))
    }

    /// Moves a running sandbox back to the provisioned state.
    pub fn stop(&self, sandbox_id: &SandboxId) -> Result<StopResult> {
        self.backend.stop(sandbox_id)
    }

    /// Releases a provisioned sandbox. The sandbox ID becomes stale.
    pub fn deprovision(&self, sandbox_id: &SandboxId) -> Result<DeprovisionResult> {
        self.backend.deprovision(sandbox_id)
    }

    /// Checks an exec request the way [`AciEdgeSandbox::exec`] does, without running anything.
    ///
    /// Reports structural problems first, then unsupported features and deterministic backend
    /// policies. Host-dependent checks happen only in [`AciEdgeSandbox::exec`].
    pub fn validate_exec(&self, request: &ExecRequest) -> Result<()> {
        validate::exec_structure(request)?;
        validate::exec_capabilities(request, &self.backend.capabilities())?;
        self.backend.validate_exec(request)
    }

    #[cfg_attr(not(feature = "async"), allow(dead_code))]
    pub(crate) fn exec_with_io(
        &self,
        sandbox_id: &SandboxId,
        request: &ExecRequest,
        io: ExecIo,
    ) -> Result<Box<dyn ExecControl>> {
        self.validate_exec(request)?;
        self.backend.exec(sandbox_id, request, io)
    }
}

impl fmt::Debug for AciEdgeSandbox {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AciEdgeSandbox")
            .field("backend", &self.backend)
            .finish()
    }
}

fn pipe_error(error: io::Error) -> Error {
    Error::backend_error("failed to create an execution pipe").with_source(error)
}

fn thread_error(error: io::Error) -> Error {
    Error::backend_error("failed to start an execution stream thread").with_source(error)
}
