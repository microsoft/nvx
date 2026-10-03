//! Tokio wrappers around the synchronous core.

use std::fmt;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::thread;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::backend::{ExecControl, ExecIo};
use crate::capabilities::Capabilities;
use crate::client::AciEdgeSandbox;
use crate::error::{Error, Result};
use crate::exec::{Canceller, ExecOutcome, ExecOutput};
use crate::id::SandboxId;
use crate::input::InputSource;
use crate::model::{
    DeprovisionResult, ExecRequest, ProvisionRequest, ProvisionResult, StartResult, StdinMode,
    StopResult,
};
use crate::stream::{self, QueueReader, QueueWriter};

/// Asynchronous counterpart of [`AciEdgeSandbox`].
///
/// Lifecycle calls run on Tokio's blocking thread pool, so they must be awaited inside a Tokio
/// runtime. Output streams are delivered in-process without extra operating-system pipes.
#[derive(Clone, Debug)]
pub struct AsyncAciEdgeSandbox {
    nvx: AciEdgeSandbox,
}

impl AsyncAciEdgeSandbox {
    /// Wraps a synchronous client.
    pub fn new(nvx: AciEdgeSandbox) -> Self {
        Self { nvx }
    }

    /// Returns the underlying synchronous client.
    pub fn blocking(&self) -> &AciEdgeSandbox {
        &self.nvx
    }

    /// Returns the features the backend honors.
    pub fn capabilities(&self) -> Capabilities {
        self.nvx.capabilities()
    }

    /// Checks that the backend's runtime dependencies are present.
    pub async fn probe(&self) -> Result<()> {
        let nvx = self.nvx.clone();
        blocking(move || nvx.probe()).await
    }

    /// Allocates a sandbox without starting it.
    pub async fn provision(&self, request: ProvisionRequest) -> Result<ProvisionResult> {
        let nvx = self.nvx.clone();
        blocking(move || nvx.provision(&request)).await
    }

    /// Moves a provisioned sandbox to the running state.
    pub async fn start(&self, sandbox_id: SandboxId) -> Result<StartResult> {
        let nvx = self.nvx.clone();
        blocking(move || nvx.start(&sandbox_id)).await
    }

    /// Starts a workload in a running sandbox and returns its live streams.
    pub async fn exec(
        &self,
        sandbox_id: SandboxId,
        request: ExecRequest,
    ) -> Result<AsyncExecution> {
        let (stdout_sink, stdout) = stream::queue();
        let (stderr_sink, stderr) = stream::queue();
        let (backend_stdin, stdin) = match request.stdin {
            StdinMode::Null => (None, None),
            StdinMode::Piped => {
                let (writer, reader) = stream::queue();
                (
                    Some(InputSource::from_queue(reader)),
                    Some(InputStream {
                        writer: Some(writer),
                    }),
                )
            }
        };
        let io = ExecIo {
            stdout: Box::new(stdout_sink),
            stderr: Box::new(stderr_sink),
            stdin: backend_stdin,
        };
        let nvx = self.nvx.clone();
        let control = blocking(move || nvx.exec_with_io(&sandbox_id, &request, io)).await?;
        Ok(AsyncExecution {
            stdout: Some(OutputStream { reader: stdout }),
            stderr: Some(OutputStream { reader: stderr }),
            stdin,
            control: Arc::from(control),
        })
    }

    /// Moves a running sandbox back to the provisioned state.
    pub async fn stop(&self, sandbox_id: SandboxId) -> Result<StopResult> {
        let nvx = self.nvx.clone();
        blocking(move || nvx.stop(&sandbox_id)).await
    }

    /// Releases a provisioned sandbox. The sandbox ID becomes stale.
    pub async fn deprovision(&self, sandbox_id: SandboxId) -> Result<DeprovisionResult> {
        let nvx = self.nvx.clone();
        blocking(move || nvx.deprovision(&sandbox_id)).await
    }
}

impl From<AciEdgeSandbox> for AsyncAciEdgeSandbox {
    fn from(nvx: AciEdgeSandbox) -> Self {
        Self::new(nvx)
    }
}

async fn blocking<T, F>(operation: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| Error::backend_error("sandbox blocking task failed").with_source(error))?
}

/// A live execution returned by [`AsyncAciEdgeSandbox::exec`].
pub struct AsyncExecution {
    stdout: Option<OutputStream>,
    stderr: Option<OutputStream>,
    stdin: Option<InputStream>,
    control: Arc<dyn ExecControl>,
}

impl AsyncExecution {
    /// Takes the workload's standard output stream.
    pub fn take_stdout(&mut self) -> Option<OutputStream> {
        self.stdout.take()
    }

    /// Takes the workload's standard error stream.
    pub fn take_stderr(&mut self) -> Option<OutputStream> {
        self.stderr.take()
    }

    /// Takes the workload's standard input stream. Present only for
    /// [`StdinMode::Piped`] requests; shut it down or drop it to deliver end-of-file.
    pub fn take_stdin(&mut self) -> Option<InputStream> {
        self.stdin.take()
    }

    /// Returns a handle that cancels this execution.
    pub fn canceller(&self) -> Canceller {
        Canceller::new(Arc::clone(&self.control))
    }

    /// Waits for the terminal outcome, discarding any output stream that was not taken.
    pub async fn wait(mut self) -> Result<ExecOutcome> {
        self.stdin = None;
        self.stdout = None;
        self.stderr = None;
        let control = Arc::clone(&self.control);
        blocking(move || control.wait()).await
    }

    /// Collects every untaken output stream and waits for the terminal outcome.
    pub async fn wait_with_output(mut self) -> Result<ExecOutput> {
        self.stdin = None;
        let stdout = self.stdout.take();
        let stderr = self.stderr.take();
        let control = Arc::clone(&self.control);
        blocking(move || {
            let stderr = match stderr {
                Some(stream) => Some(
                    thread::Builder::new()
                        .name("nvx-exec-collect".to_owned())
                        .spawn(move || stream.reader.read_to_end_blocking())
                        .map_err(|error| {
                            Error::backend_error("failed to start an output collector thread")
                                .with_source(error)
                        })?,
                ),
                None => None,
            };
            let stdout = stdout
                .map(|stream| stream.reader.read_to_end_blocking())
                .unwrap_or_default();
            let outcome = control.wait();
            let stderr = match stderr {
                Some(collector) => collector
                    .join()
                    .map_err(|_| Error::backend_error("output collector thread panicked"))?,
                None => Vec::new(),
            };
            Ok(ExecOutput {
                outcome: outcome?,
                stdout,
                stderr,
            })
        })
        .await
    }
}

impl fmt::Debug for AsyncExecution {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AsyncExecution")
            .field("stdout", &self.stdout.is_some())
            .field("stderr", &self.stderr.is_some())
            .field("stdin", &self.stdin.is_some())
            .finish_non_exhaustive()
    }
}

/// Asynchronous output stream of an [`AsyncExecution`].
pub struct OutputStream {
    reader: QueueReader,
}

impl AsyncRead for OutputStream {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let unfilled = buffer.initialize_unfilled();
        match self.reader.poll_read(context, unfilled) {
            Poll::Ready(count) => {
                buffer.advance(count);
                Poll::Ready(Ok(()))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl fmt::Debug for OutputStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OutputStream")
            .finish_non_exhaustive()
    }
}

/// Asynchronous standard input stream of an [`AsyncExecution`].
pub struct InputStream {
    writer: Option<QueueWriter>,
}

impl AsyncWrite for InputStream {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &self.writer {
            Some(writer) => writer.poll_write(context, buffer),
            None => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.writer = None;
        Poll::Ready(Ok(()))
    }
}

impl fmt::Debug for InputStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InputStream")
            .field("open", &self.writer.is_some())
            .finish()
    }
}
