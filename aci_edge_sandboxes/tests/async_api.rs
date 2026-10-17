//! Exercises the Tokio wrappers through the in-memory `MockBackend`.
#![cfg(all(feature = "async", feature = "testing"))]

use std::sync::{Mutex, mpsc};
use std::time::Duration;

use aci_edge_sandboxes::testing::{MockBackend, MockExec};
use aci_edge_sandboxes::{
    AciEdgeSandbox, AsyncAciEdgeSandbox, Backend, Capabilities, DeprovisionResult, ErrorCode,
    ExecControl, ExecIo, ExecOutcome, ExecRequest, ProvisionRequest, ProvisionResult, Result,
    SandboxId, StartResult, StdinMode, StopResult,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn request() -> ProvisionRequest {
    ProvisionRequest::new()
}

#[test]
fn async_lifecycle_streams_output() {
    runtime().block_on(async {
        let nvx = AsyncAciEdgeSandbox::new(AciEdgeSandbox::new(
            MockBackend::new().with_exec_handler(|_| MockExec {
                stdout: b"out".to_vec(),
                stderr: b"err".to_vec(),
                outcome: Some(ExecOutcome::Exited(7)),
                ..MockExec::default()
            }),
        ));
        let sandbox_id = nvx.provision(request()).await.unwrap().sandbox_id;
        assert_eq!(
            nvx.exec(sandbox_id.clone(), ExecRequest::command_line("x"))
                .await
                .unwrap_err()
                .code(),
            ErrorCode::NotStarted
        );
        nvx.start(sandbox_id.clone()).await.unwrap();

        let mut execution = nvx
            .exec(sandbox_id.clone(), ExecRequest::command_line("x"))
            .await
            .unwrap();
        let mut stdout = execution.take_stdout().unwrap();
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).await.unwrap();
        assert_eq!(output, b"out");
        assert_eq!(execution.wait().await.unwrap(), ExecOutcome::Exited(7));

        let collected = nvx
            .exec(sandbox_id.clone(), ExecRequest::command_line("x"))
            .await
            .unwrap()
            .wait_with_output()
            .await
            .unwrap();
        assert_eq!(collected.stdout, b"out");
        assert_eq!(collected.stderr, b"err");

        nvx.stop(sandbox_id.clone()).await.unwrap();
        nvx.deprovision(sandbox_id).await.unwrap();
    });
}

#[test]
fn async_stdin_and_cancellation() {
    runtime().block_on(async {
        let nvx = AsyncAciEdgeSandbox::new(AciEdgeSandbox::new(
            MockBackend::new().with_exec_handler(|request| MockExec {
                echo_stdin: request.stdin == StdinMode::Piped,
                duration: if request.stdin == StdinMode::Piped {
                    Duration::ZERO
                } else {
                    Duration::from_secs(30)
                },
                ..MockExec::default()
            }),
        ));
        let sandbox_id = nvx.provision(request()).await.unwrap().sandbox_id;
        nvx.start(sandbox_id.clone()).await.unwrap();

        let mut execution = nvx
            .exec(
                sandbox_id.clone(),
                ExecRequest::command_line("cat").with_stdin(StdinMode::Piped),
            )
            .await
            .unwrap();
        let mut stdin = execution.take_stdin().unwrap();
        stdin.write_all(b"ping").await.unwrap();
        stdin.shutdown().await.unwrap();
        let output = execution.wait_with_output().await.unwrap();
        assert_eq!(output.stdout, b"ping");

        let execution = nvx
            .exec(sandbox_id.clone(), ExecRequest::command_line("sleep"))
            .await
            .unwrap();
        execution.canceller().cancel().unwrap();
        assert_eq!(execution.wait().await.unwrap(), ExecOutcome::Cancelled);
    });
}

/// Delegates to a [`MockBackend`], but holds each exec until the test releases it, as a sandbox
/// whose only execution slot is busy does, and reports every cancellation request.
#[derive(Debug)]
struct HeldBackend {
    inner: MockBackend,
    released: Mutex<mpsc::Receiver<()>>,
    cancelled: mpsc::Sender<()>,
}

impl Backend for HeldBackend {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn probe(&self) -> Result<()> {
        self.inner.probe()
    }

    fn provision(&self, request: &ProvisionRequest) -> Result<ProvisionResult> {
        self.inner.provision(request)
    }

    fn start(&self, sandbox_id: &SandboxId) -> Result<StartResult> {
        self.inner.start(sandbox_id)
    }

    fn exec(
        &self,
        sandbox_id: &SandboxId,
        request: &ExecRequest,
        io: ExecIo,
    ) -> Result<Box<dyn ExecControl>> {
        self.released.lock().unwrap().recv().unwrap();
        let control = self.inner.exec(sandbox_id, request, io)?;
        Ok(Box::new(ReportedCancellation {
            control,
            cancelled: self.cancelled.clone(),
        }))
    }

    fn stop(&self, sandbox_id: &SandboxId) -> Result<StopResult> {
        self.inner.stop(sandbox_id)
    }

    fn deprovision(&self, sandbox_id: &SandboxId) -> Result<DeprovisionResult> {
        self.inner.deprovision(sandbox_id)
    }
}

struct ReportedCancellation {
    control: Box<dyn ExecControl>,
    cancelled: mpsc::Sender<()>,
}

impl ExecControl for ReportedCancellation {
    fn wait(&self) -> Result<ExecOutcome> {
        self.control.wait()
    }

    fn cancel(&self) -> Result<()> {
        let _ = self.cancelled.send(());
        self.control.cancel()
    }
}

#[test]
fn dropping_an_exec_future_cancels_its_workload() {
    runtime().block_on(async {
        let (release, released) = mpsc::channel();
        let (cancelled, cancellations) = mpsc::channel();
        let client = AsyncAciEdgeSandbox::new(AciEdgeSandbox::new(HeldBackend {
            inner: MockBackend::new().with_exec_handler(|_| MockExec {
                duration: Duration::from_secs(60),
                ..MockExec::default()
            }),
            released: Mutex::new(released),
            cancelled,
        }));
        let sandbox_id = client.provision(request()).await.unwrap().sandbox_id;
        client.start(sandbox_id.clone()).await.unwrap();

        // The caller gives up while the backend still waits to start the workload.
        let abandoned = client.exec(sandbox_id.clone(), ExecRequest::command_line("sleep"));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), abandoned)
                .await
                .is_err()
        );
        // The workload starts anyway, and since nothing can reach it, it is cancelled at once.
        release.send(()).unwrap();
        cancellations
            .recv_timeout(Duration::from_secs(10))
            .expect("the abandoned workload must be cancelled");

        // A caller that receives its execution keeps control of the workload.
        release.send(()).unwrap();
        let execution = client
            .exec(sandbox_id.clone(), ExecRequest::command_line("sleep"))
            .await
            .unwrap();
        assert!(cancellations.try_recv().is_err());
        execution.canceller().cancel().unwrap();
        cancellations.try_recv().unwrap();
        assert_eq!(execution.wait().await.unwrap(), ExecOutcome::Cancelled);
        assert!(cancellations.try_recv().is_err());
    });
}

#[test]
fn async_cancel_and_timeout_close_retained_input_streams() {
    runtime().block_on(async {
        let client = AsyncAciEdgeSandbox::new(AciEdgeSandbox::new(
            MockBackend::new().with_exec_handler(|_| MockExec {
                echo_stdin: true,
                ..MockExec::default()
            }),
        ));
        let sandbox_id = client.provision(request()).await.unwrap().sandbox_id;
        client.start(sandbox_id.clone()).await.unwrap();
        for cancelled in [true, false] {
            let mut request = ExecRequest::command_line("cat").with_stdin(StdinMode::Piped);
            // A workload timeout could expire before the cancellation arrives, so only the timeout
            // case gets one; the outer deadline below bounds the cancelled case.
            request.process.timeout = (!cancelled).then_some(Duration::from_millis(100));
            let mut execution = client.exec(sandbox_id.clone(), request).await.unwrap();
            let mut stdin = execution.take_stdin().unwrap();
            if cancelled {
                execution.canceller().cancel().unwrap();
            }
            let outcome = tokio::time::timeout(Duration::from_secs(2), execution.wait())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                outcome,
                if cancelled {
                    ExecOutcome::Cancelled
                } else {
                    ExecOutcome::TimedOut
                }
            );
            assert_eq!(
                stdin
                    .write_all(b"after completion")
                    .await
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::BrokenPipe
            );
        }
    });
}

/// Delegates to a [`MockBackend`], but keeps each execution's output open until it is waited on,
/// as the native agent does while the workload runs. `wait` then applies the timeout and closes
/// the output sinks, the same way the native agent does on timeout.
#[derive(Debug)]
struct OpenUntilWaitBackend {
    inner: MockBackend,
}

impl Backend for OpenUntilWaitBackend {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn probe(&self) -> Result<()> {
        self.inner.probe()
    }

    fn provision(&self, request: &ProvisionRequest) -> Result<ProvisionResult> {
        self.inner.provision(request)
    }

    fn start(&self, sandbox_id: &SandboxId) -> Result<StartResult> {
        self.inner.start(sandbox_id)
    }

    fn exec(
        &self,
        _sandbox_id: &SandboxId,
        request: &ExecRequest,
        io: ExecIo,
    ) -> Result<Box<dyn ExecControl>> {
        Ok(Box::new(OpenUntilWait {
            io: Mutex::new(io),
            timeout: request.process.timeout.expect("the test sets a timeout"),
        }))
    }

    fn stop(&self, sandbox_id: &SandboxId) -> Result<StopResult> {
        self.inner.stop(sandbox_id)
    }

    fn deprovision(&self, sandbox_id: &SandboxId) -> Result<DeprovisionResult> {
        self.inner.deprovision(sandbox_id)
    }
}

struct OpenUntilWait {
    io: Mutex<ExecIo>,
    timeout: Duration,
}

impl ExecControl for OpenUntilWait {
    fn wait(&self) -> Result<ExecOutcome> {
        std::thread::sleep(self.timeout);
        let io = self.io.lock().unwrap();
        for closer in [io.stdout.close_handle(), io.stderr.close_handle()]
            .into_iter()
            .flatten()
        {
            closer.close();
        }
        Ok(ExecOutcome::TimedOut)
    }

    fn cancel(&self) -> Result<()> {
        Ok(())
    }
}

#[test]
fn async_wait_with_output_honors_the_workload_timeout() {
    // Run on a separate thread so a regression fails the test instead of hanging the runtime,
    // whose shutdown waits for the blocked output collector.
    let (done, outcome) = mpsc::channel();
    std::thread::spawn(move || {
        runtime().block_on(async {
            let client = AsyncAciEdgeSandbox::new(AciEdgeSandbox::new(OpenUntilWaitBackend {
                inner: MockBackend::new(),
            }));
            let sandbox_id = client.provision(request()).await.unwrap().sandbox_id;
            client.start(sandbox_id.clone()).await.unwrap();
            let mut exec_request = ExecRequest::command_line("sleep");
            exec_request.process.timeout = Some(Duration::from_millis(100));
            let execution = client.exec(sandbox_id, exec_request).await.unwrap();
            let output = execution.wait_with_output().await.unwrap();
            done.send(output.outcome).unwrap();
        });
    });
    // Before the fix, collection read stdout to end-of-stream before `wait` ran, so no outcome
    // arrived and this deadline fired.
    assert_eq!(
        outcome
            .recv_timeout(Duration::from_secs(10))
            .expect("wait_with_output ignored the workload timeout"),
        ExecOutcome::TimedOut
    );
}
