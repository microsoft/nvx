//! Exercises the backend-independent `AciEdgeSandbox` facade through the in-memory `MockBackend`.
#![cfg(feature = "testing")]

use std::io::{Read, Write};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use aci_edge_sandboxes::testing::{MockBackend, MockExec};
use aci_edge_sandboxes::{
    Access, AciEdgeSandbox, Capabilities, EgressPolicy, ErrorCode, ExecOutcome, ExecRequest,
    FilesystemPolicy, NetworkPolicy, NetworkRule, Protocol, ProvisionRequest, SandboxId, StdinMode,
};

/// Longest time a scenario that must not hang may take.
const SCENARIO_LIMIT: Duration = Duration::from_secs(30);

/// Runs `work` on another thread, so that a hang fails the test instead of stalling the suite.
fn within<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let _ = sender.send(work());
    });
    match receiver.recv_timeout(SCENARIO_LIMIT) {
        Ok(value) => value,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("the scenario did not finish within {SCENARIO_LIMIT:?}")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => match worker.join() {
            Err(panic) => std::panic::resume_unwind(panic),
            Ok(()) => unreachable!("the scenario sends its result before it ends"),
        },
    }
}

fn absolute(name: &str) -> String {
    if cfg!(windows) {
        format!("C:\\nvx\\{name}")
    } else {
        format!("/nvx/{name}")
    }
}

fn request() -> ProvisionRequest {
    ProvisionRequest::new()
}

/// A request that a backend without filesystem capabilities cannot honor.
fn mapped() -> ProvisionRequest {
    ProvisionRequest::new().with_filesystem(FilesystemPolicy {
        readonly_paths: vec![absolute("source").into()],
        ..FilesystemPolicy::default()
    })
}

fn running(nvx: &AciEdgeSandbox) -> SandboxId {
    let sandbox_id = nvx.provision(&request()).unwrap().sandbox_id;
    nvx.start(&sandbox_id).unwrap();
    sandbox_id
}

#[test]
fn lifecycle_follows_the_state_machine() {
    let backend = MockBackend::new();
    let nvx = AciEdgeSandbox::new(backend);
    let sandbox_id = nvx.provision(&request()).unwrap().sandbox_id;
    assert_eq!(
        nvx.exec(&sandbox_id, &ExecRequest::command_line("echo"))
            .unwrap_err()
            .code(),
        ErrorCode::NotStarted
    );
    nvx.start(&sandbox_id).unwrap();
    let output = nvx
        .exec(&sandbox_id, &ExecRequest::command_line("echo hello"))
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert_eq!(output.stdout, b"echo hello\n");
    assert!(output.outcome.success());
    assert_eq!(
        nvx.deprovision(&sandbox_id).unwrap_err().code(),
        ErrorCode::AlreadyStarted
    );
    nvx.stop(&sandbox_id).unwrap();
    assert_eq!(
        nvx.stop(&sandbox_id).unwrap_err().code(),
        ErrorCode::AlreadyStopped
    );
    nvx.deprovision(&sandbox_id).unwrap();
    assert_eq!(
        nvx.start(&sandbox_id).unwrap_err().code(),
        ErrorCode::StaleId
    );
}

#[test]
fn structural_errors_precede_capability_errors() {
    let mut capabilities = MockBackend::full_capabilities();
    capabilities.exec.cwd = false;
    let nvx = AciEdgeSandbox::new(MockBackend::new().with_capabilities(capabilities));
    let sandbox_id = running(&nvx);
    let both = ExecRequest::command_line(" ").with_cwd("/tmp");
    assert_eq!(
        nvx.exec(&sandbox_id, &both).unwrap_err().code(),
        ErrorCode::MalformedRequest
    );
    let unsupported = ExecRequest::command_line("pwd").with_cwd("/tmp");
    assert_eq!(
        nvx.exec(&sandbox_id, &unsupported).unwrap_err().code(),
        ErrorCode::PolicyValidation
    );
}

#[test]
fn rejected_provisions_never_reach_the_backend() {
    let backend = Arc::new(MockBackend::new().with_capabilities(Capabilities::new("restricted")));
    let nvx = AciEdgeSandbox::from_shared(backend.clone());
    assert_eq!(
        nvx.provision(&mapped()).unwrap_err().code(),
        ErrorCode::PolicyValidation
    );
    assert_eq!(backend.sandbox_count(), 0);
}

#[test]
fn the_default_mock_supports_egress_rules() {
    let backend = Arc::new(MockBackend::new());
    let nvx = AciEdgeSandbox::from_shared(backend.clone());
    let request = ProvisionRequest::new().with_network(NetworkPolicy {
        egress: EgressPolicy::new(Access::Deny)
            .with_allow(NetworkRule::to("10.0.0.0/8").on_port(Protocol::Tcp, 443))
            .with_deny(NetworkRule::to("10.1.0.0/16")),
        ..NetworkPolicy::deny_all()
    });
    nvx.validate_provision(&request).unwrap();
    let sandbox_id = nvx.provision(&request).unwrap().sandbox_id;
    assert_eq!(backend.provision_request(&sandbox_id), Some(request));
}

#[test]
fn validation_alone_runs_nothing() {
    let backend = Arc::new(MockBackend::new());
    let nvx = AciEdgeSandbox::from_shared(backend.clone());
    nvx.validate_provision(&request()).unwrap();
    assert_eq!(backend.sandbox_count(), 0);
    nvx.validate_exec(&ExecRequest::command_line("true"))
        .unwrap();
    assert_eq!(
        nvx.validate_exec(&ExecRequest::command_line(" "))
            .unwrap_err()
            .code(),
        ErrorCode::MalformedRequest
    );

    let restricted =
        AciEdgeSandbox::new(MockBackend::new().with_capabilities(Capabilities::new("restricted")));
    assert_eq!(
        restricted.validate_provision(&mapped()).unwrap_err().code(),
        ErrorCode::PolicyValidation
    );
}

#[test]
fn piped_stdin_reaches_the_workload() {
    let nvx = AciEdgeSandbox::new(MockBackend::new().with_exec_handler(|_| MockExec {
        echo_stdin: true,
        ..MockExec::default()
    }));
    let sandbox_id = running(&nvx);
    let mut execution = nvx
        .exec(
            &sandbox_id,
            &ExecRequest::command_line("cat").with_stdin(StdinMode::Piped),
        )
        .unwrap();
    let mut stdin = execution.take_stdin().unwrap();
    stdin.write_all(b"from the caller").unwrap();
    drop(stdin);
    let mut stdout = execution.take_stdout().unwrap();
    let mut output = Vec::new();
    stdout.read_to_end(&mut output).unwrap();
    assert_eq!(output, b"from the caller");
    assert!(execution.wait().unwrap().success());
}

#[test]
fn cancellation_and_timeouts_are_distinct_outcomes() {
    let nvx = AciEdgeSandbox::new(MockBackend::new().with_exec_handler(|_| MockExec {
        duration: Duration::from_secs(30),
        ..MockExec::default()
    }));
    let sandbox_id = running(&nvx);
    let execution = nvx
        .exec(&sandbox_id, &ExecRequest::command_line("sleep"))
        .unwrap();
    let canceller = execution.canceller();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        canceller.cancel().unwrap();
    });
    assert_eq!(execution.wait().unwrap(), ExecOutcome::Cancelled);
    let timed_out = nvx
        .exec(
            &sandbox_id,
            &ExecRequest::command_line("sleep").with_timeout(Duration::from_millis(50)),
        )
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(timed_out, ExecOutcome::TimedOut);
}

/// A mock whose workloads copy piped standard input to standard output.
fn echoing(duration: Duration) -> AciEdgeSandbox {
    AciEdgeSandbox::new(MockBackend::new().with_exec_handler(move |_| MockExec {
        echo_stdin: true,
        duration,
        ..MockExec::default()
    }))
}

fn cat() -> ExecRequest {
    ExecRequest::command_line("cat").with_stdin(StdinMode::Piped)
}

#[test]
fn cancellation_interrupts_a_workload_waiting_for_stdin() {
    let nvx = echoing(Duration::ZERO);
    let sandbox_id = running(&nvx);
    within(move || {
        let mut execution = nvx.exec(&sandbox_id, &cat()).unwrap();
        // The caller keeps the pipe open, so the workload never reads end-of-file.
        let mut stdin = execution.take_stdin().unwrap();
        let mut stdout = execution.take_stdout().unwrap();
        stdin.write_all(b"ping").unwrap();
        let mut echoed = [0u8; 4];
        stdout.read_exact(&mut echoed).unwrap();
        assert_eq!(&echoed, b"ping");

        execution.canceller().cancel().unwrap();
        assert_eq!(execution.wait().unwrap(), ExecOutcome::Cancelled);
        // The streams are closed by the time the outcome is reported.
        let mut rest = Vec::new();
        stdout.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
        assert_eq!(
            stdin.write_all(b"after cancellation").unwrap_err().kind(),
            std::io::ErrorKind::BrokenPipe
        );
    });
}

#[test]
fn the_timeout_interrupts_a_workload_waiting_for_stdin() {
    let nvx = echoing(Duration::ZERO);
    let sandbox_id = running(&nvx);
    within(move || {
        let request = cat().with_timeout(Duration::from_millis(100));
        let mut execution = nvx.exec(&sandbox_id, &request).unwrap();
        let mut stdin = execution.take_stdin().unwrap();
        let output = execution.wait_with_output().unwrap();
        assert_eq!(output.outcome, ExecOutcome::TimedOut);
        assert!(output.stdout.is_empty());
        assert_eq!(
            stdin.write_all(b"after timeout").unwrap_err().kind(),
            std::io::ErrorKind::BrokenPipe
        );
    });
}

#[test]
fn completed_echoes_close_input_with_all_writers_retained() {
    within(move || {
        let client = echoing(Duration::ZERO);
        let sandbox_id = running(&client);
        let mut retained = Vec::new();
        for index in 0..16 {
            let cancelled = index % 2 == 0;
            let mut request = cat();
            // A workload timeout could expire before a cancellation arrives, so only the timeout
            // cases get one; `within` bounds the cancelled cases.
            request.process.timeout = (!cancelled).then_some(Duration::from_millis(20));
            let mut execution = client.exec(&sandbox_id, &request).unwrap();
            retained.push(execution.take_stdin().unwrap());
            let expected = if cancelled {
                execution.canceller().cancel().unwrap();
                ExecOutcome::Cancelled
            } else {
                ExecOutcome::TimedOut
            };
            assert_eq!(execution.wait().unwrap(), expected);
        }
        for mut writer in retained {
            assert_eq!(
                writer.write(b"x").unwrap_err().kind(),
                std::io::ErrorKind::BrokenPipe
            );
        }
    });
}

#[test]
fn the_timeout_covers_the_wait_for_stdin_and_the_run_time() {
    let nvx = echoing(Duration::from_millis(400));
    let sandbox_id = running(&nvx);
    within(move || {
        let request = cat().with_timeout(Duration::from_millis(600));
        let mut execution = nvx.exec(&sandbox_id, &request).unwrap();
        let stdin = execution.take_stdin().unwrap();
        // End-of-file arrives with at most 200 ms of the timeout left, which is less than the
        // run time that remains.
        thread::sleep(Duration::from_millis(400));
        drop(stdin);
        assert_eq!(execution.wait().unwrap(), ExecOutcome::TimedOut);
    });
}

#[test]
fn clients_are_shared_across_threads() {
    let nvx = AciEdgeSandbox::new(MockBackend::new());
    let sandbox_id = running(&nvx);
    let workers: Vec<_> = (0..4)
        .map(|index| {
            let nvx = nvx.clone();
            let sandbox_id = sandbox_id.clone();
            thread::spawn(move || {
                nvx.exec(
                    &sandbox_id,
                    &ExecRequest::argv(["/bin/echo", &index.to_string()]),
                )
                .unwrap()
                .wait_with_output()
                .unwrap()
                .stdout
            })
        })
        .collect();
    for (index, worker) in workers.into_iter().enumerate() {
        assert_eq!(
            worker.join().unwrap(),
            format!("/bin/echo {index}\n").into_bytes()
        );
    }
}
