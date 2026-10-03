//! Exercises the Tokio wrappers through the in-memory `MockBackend`.
#![cfg(all(feature = "async", feature = "testing"))]

use std::time::Duration;

use aci_edge_sandboxes::testing::{MockBackend, MockExec};
use aci_edge_sandboxes::{
    AciEdgeSandbox, AsyncAciEdgeSandbox, ErrorCode, ExecOutcome, ExecRequest, ProvisionRequest,
    StdinMode,
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
