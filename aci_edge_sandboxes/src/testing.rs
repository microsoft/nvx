//! Test doubles for code that consumes this crate.
//!
//! [`MockBackend`] implements the complete lifecycle state machine in memory, with the same error
//! codes as real backends, and runs scripted executions instead of workloads.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, Read};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crate::backend::{Backend, ExecControl, ExecIo, OutputSink};
use crate::capabilities::Capabilities;
use crate::error::{Error, Result};
use crate::exec::{Completion, ExecOutcome};
use crate::id::SandboxId;
use crate::input::InputSource;
use crate::model::{
    Command, DeprovisionResult, ExecRequest, ProvisionRequest, ProvisionResult, StartResult,
    StopResult,
};

type ExecHandler = dyn Fn(&ExecRequest) -> MockExec + Send + Sync;

/// Scripted behavior of one mock execution.
///
/// Cancellation and the request's timeout close the output streams, so writes that wait for a
/// consumer that stopped reading cannot delay the outcome.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MockExec {
    /// Bytes written to standard output when the execution starts.
    pub stdout: Vec<u8>,
    /// Bytes written to standard error when the execution starts.
    pub stderr: Vec<u8>,
    /// Copies piped standard input to standard output until end-of-file, before the simulated run
    /// time starts. The request's timeout and cancellation interrupt the wait for input.
    pub echo_stdin: bool,
    /// Simulated run time. The request's timeout, which also covers the scripted output and the
    /// echo of standard input, and cancellation interrupt it.
    pub duration: Duration,
    /// Outcome after the simulated run time. `None` means [`ExecOutcome::Exited`] with status 0.
    pub outcome: Option<ExecOutcome>,
}

struct MockSandbox {
    request: ProvisionRequest,
    running: bool,
}

/// In-memory [`Backend`] for tests.
///
/// By default every feature is supported and each execution echoes its command to standard
/// output. Use [`MockBackend::with_exec_handler`] to script executions.
pub struct MockBackend {
    sandboxes: Mutex<HashMap<SandboxId, MockSandbox>>,
    capabilities: Capabilities,
    handler: Arc<ExecHandler>,
}

impl MockBackend {
    /// Creates a mock backend with every feature supported.
    pub fn new() -> Self {
        Self {
            sandboxes: Mutex::new(HashMap::new()),
            capabilities: Self::full_capabilities(),
            handler: Arc::new(|request: &ExecRequest| MockExec {
                stdout: echo(&request.process.command).into_bytes(),
                ..MockExec::default()
            }),
        }
    }

    /// Returns a capability set in which every feature is supported.
    pub fn full_capabilities() -> Capabilities {
        let mut capabilities = Capabilities::new("mock");
        let exec = &mut capabilities.exec;
        exec.command_line = true;
        exec.argv = true;
        exec.stdin = true;
        exec.cancel = true;
        exec.cwd = true;
        exec.env = true;
        exec.clear_default_env = true;
        exec.concurrent = true;
        let network = &mut capabilities.network;
        network.egress_allow = true;
        network.egress_deny = true;
        network.ingress_allow = true;
        network.ingress_deny = true;
        network.host_loopback_allow = true;
        network.host_loopback_deny = true;
        network.egress_rules = true;
        let filesystem = &mut capabilities.filesystem;
        filesystem.readonly_paths = true;
        filesystem.readwrite_paths = true;
        filesystem.denied_paths = true;
        capabilities
    }

    /// Replaces the advertised capabilities.
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Scripts every execution with `handler`.
    #[must_use]
    pub fn with_exec_handler<F>(mut self, handler: F) -> Self
    where
        F: Fn(&ExecRequest) -> MockExec + Send + Sync + 'static,
    {
        self.handler = Arc::new(handler);
        self
    }

    /// Returns the number of provisioned sandboxes.
    pub fn sandbox_count(&self) -> usize {
        self.lock().len()
    }

    /// Returns whether a sandbox is running, or `None` when it is not provisioned.
    pub fn is_running(&self, sandbox_id: &SandboxId) -> Option<bool> {
        self.lock().get(sandbox_id).map(|sandbox| sandbox.running)
    }

    /// Returns the request that provisioned a sandbox.
    pub fn provision_request(&self, sandbox_id: &SandboxId) -> Option<ProvisionRequest> {
        self.lock()
            .get(sandbox_id)
            .map(|sandbox| sandbox.request.clone())
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<SandboxId, MockSandbox>> {
        self.sandboxes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for MockBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MockBackend")
            .field("sandboxes", &self.sandbox_count())
            .finish_non_exhaustive()
    }
}

fn echo(command: &Command) -> String {
    match command {
        Command::CommandLine(command_line) => format!("{command_line}\n"),
        Command::Argv(argv) => format!("{}\n", argv.join(" ")),
    }
}

fn stale(sandbox_id: &SandboxId) -> Error {
    Error::stale_id(format!("sandbox {sandbox_id} is not provisioned"))
}

impl Backend for MockBackend {
    fn name(&self) -> &str {
        "mock"
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    fn probe(&self) -> Result<()> {
        Ok(())
    }

    fn provision(&self, request: &ProvisionRequest) -> Result<ProvisionResult> {
        let sandbox_id = SandboxId::generate()?;
        self.lock().insert(
            sandbox_id.clone(),
            MockSandbox {
                request: request.clone(),
                running: false,
            },
        );
        Ok(ProvisionResult {
            sandbox_id,
            metadata: None,
        })
    }

    fn start(&self, sandbox_id: &SandboxId) -> Result<StartResult> {
        let mut sandboxes = self.lock();
        let sandbox = sandboxes
            .get_mut(sandbox_id)
            .ok_or_else(|| stale(sandbox_id))?;
        if sandbox.running {
            return Err(Error::already_started(format!(
                "sandbox {sandbox_id} is already running"
            )));
        }
        sandbox.running = true;
        Ok(StartResult::default())
    }

    fn exec(
        &self,
        sandbox_id: &SandboxId,
        request: &ExecRequest,
        io: ExecIo,
    ) -> Result<Box<dyn ExecControl>> {
        {
            let sandboxes = self.lock();
            let sandbox = sandboxes.get(sandbox_id).ok_or_else(|| stale(sandbox_id))?;
            if !sandbox.running {
                return Err(Error::not_started(format!(
                    "sandbox {sandbox_id} is not running"
                )));
            }
        }
        let script = (self.handler)(request);
        let timeout = request.process.timeout.filter(|timeout| !timeout.is_zero());
        let state = Arc::new(MockExecState::default());
        let worker_state = Arc::clone(&state);
        thread::Builder::new()
            .name("aci-edge-sandboxes-mock-exec".to_owned())
            .spawn(move || {
                let outcome = run_script(&worker_state, script, timeout, io);
                worker_state.finish(outcome);
            })
            .map_err(|error| {
                Error::backend_error("failed to start a mock execution").with_source(error)
            })?;
        Ok(Box::new(MockExecution { state }))
    }

    fn stop(&self, sandbox_id: &SandboxId) -> Result<StopResult> {
        let mut sandboxes = self.lock();
        let sandbox = sandboxes
            .get_mut(sandbox_id)
            .ok_or_else(|| stale(sandbox_id))?;
        if !sandbox.running {
            return Err(Error::already_stopped(format!(
                "sandbox {sandbox_id} is not running"
            )));
        }
        sandbox.running = false;
        Ok(StopResult::default())
    }

    fn deprovision(&self, sandbox_id: &SandboxId) -> Result<DeprovisionResult> {
        let mut sandboxes = self.lock();
        let sandbox = sandboxes.get(sandbox_id).ok_or_else(|| stale(sandbox_id))?;
        if sandbox.running {
            return Err(Error::already_started(format!(
                "sandbox {sandbox_id} is running; stop it before deprovisioning"
            )));
        }
        sandboxes.remove(sandbox_id);
        Ok(DeprovisionResult::default())
    }
}

fn run_script(
    state: &Arc<MockExecState>,
    script: MockExec,
    timeout: Option<Duration>,
    io: ExecIo,
) -> Result<ExecOutcome> {
    let started = Instant::now();
    let ExecIo {
        stdout,
        stderr,
        mut stdin,
    } = io;
    // A workload that does not echo leaves standard input unread but open until it ends.
    let echoed = stdin.take_if(|_| script.echo_stdin);
    let input_closer = echoed.as_ref().map(InputSource::close_handle);
    let output_closers = [stdout.close_handle(), stderr.close_handle()];
    let worker = spawn_output(
        state,
        [script.stdout, script.stderr],
        (stdout, stderr),
        echoed,
    )
    .map_err(|error| {
        Error::backend_error("failed to start the mock output worker").with_source(error)
    })?;
    let wake = state.wait_for(timeout, |signals| signals.output_ended);
    if let Some(closer) = input_closer {
        closer.close();
    }
    if !matches!(wake, Wake::Ready) {
        // A consumer that retains a stream without draining it would otherwise block the worker.
        for closer in output_closers.iter().flatten() {
            closer.close();
        }
    }
    let (stdout, stderr) = worker
        .join()
        .map_err(|_| Error::backend_error("the mock output worker panicked"))?
        .map_err(|error| Error::backend_error("failed to read mock input").with_source(error))?;
    match wake {
        Wake::Ready => {}
        Wake::Cancelled => return Ok(ExecOutcome::Cancelled),
        Wake::Elapsed => return Ok(ExecOutcome::TimedOut),
    }
    let remaining = timeout.map(|timeout| timeout.saturating_sub(started.elapsed()));
    let run_time = match remaining {
        Some(remaining) if remaining < script.duration => remaining,
        _ => script.duration,
    };
    let outcome = if matches!(state.wait_for(Some(run_time), |_| false), Wake::Cancelled) {
        ExecOutcome::Cancelled
    } else if run_time < script.duration {
        ExecOutcome::TimedOut
    } else {
        script.outcome.unwrap_or(ExecOutcome::Exited(0))
    };
    drop((stdout, stderr, stdin));
    Ok(outcome)
}

/// Wakes the execution even when the output worker unwinds.
struct OutputFinished(Arc<MockExecState>);

impl Drop for OutputFinished {
    fn drop(&mut self) {
        self.0.signal(|signals| signals.output_ended = true);
    }
}

/// Standard output and standard error sinks of one execution.
type Sinks = (Box<dyn OutputSink>, Box<dyn OutputSink>);

/// Writes the scripted output, then copies `stdin`, if any, to standard output until end-of-file.
/// The worker returns the sinks, which stay open until the execution ends.
fn spawn_output(
    state: &Arc<MockExecState>,
    script: [Vec<u8>; 2],
    sinks: Sinks,
    stdin: Option<InputSource>,
) -> io::Result<thread::JoinHandle<io::Result<Sinks>>> {
    let state = Arc::clone(state);
    thread::Builder::new()
        .name("aci-edge-sandboxes-mock-output".to_owned())
        .spawn(move || {
            let _finished = OutputFinished(state);
            let [scripted_stdout, scripted_stderr] = script;
            let (mut stdout, mut stderr) = sinks;
            let _ = stdout.write(&scripted_stdout);
            let _ = stderr.write(&scripted_stderr);
            if let Some(stdin) = stdin {
                copy_input(stdin, stdout.as_mut())?;
            }
            Ok((stdout, stderr))
        })
}

fn copy_input(mut stdin: InputSource, stdout: &mut dyn OutputSink) -> io::Result<()> {
    let mut buffer = [0u8; 8192];
    loop {
        match stdin.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(count) => {
                if stdout.write(&buffer[..count]).is_err() {
                    return Ok(());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

/// Events that interrupt or advance a mock execution.
#[derive(Default)]
struct Signals {
    cancelled: bool,
    output_ended: bool,
}

/// Why [`MockExecState::wait_for`] returned.
enum Wake {
    /// The awaited condition holds.
    Ready,
    Cancelled,
    /// The time limit passed.
    Elapsed,
}

#[derive(Default)]
struct MockExecState {
    signals: Mutex<Signals>,
    signalled: Condvar,
    completion: Completion,
}

impl MockExecState {
    fn signal(&self, update: impl FnOnce(&mut Signals)) {
        update(&mut self.signals.lock().unwrap_or_else(PoisonError::into_inner));
        self.signalled.notify_all();
    }

    /// Blocks until `ready` accepts the signals, the execution is cancelled, or `limit` passes.
    /// `None` waits without a time limit. Cancellation takes precedence over the other wakeups.
    fn wait_for(&self, limit: Option<Duration>, ready: impl Fn(&Signals) -> bool) -> Wake {
        let deadline = limit.and_then(|limit| Instant::now().checked_add(limit));
        let mut signals = self.signals.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if signals.cancelled {
                return Wake::Cancelled;
            }
            if ready(&signals) {
                return Wake::Ready;
            }
            signals = match deadline {
                None => self
                    .signalled
                    .wait(signals)
                    .unwrap_or_else(PoisonError::into_inner),
                Some(deadline) => {
                    let Some(left) = deadline
                        .checked_duration_since(Instant::now())
                        .filter(|left| !left.is_zero())
                    else {
                        return Wake::Elapsed;
                    };
                    self.signalled
                        .wait_timeout(signals, left)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
            };
        }
    }

    fn finish(&self, outcome: Result<ExecOutcome>) {
        self.completion.finish(outcome);
    }
}

struct MockExecution {
    state: Arc<MockExecState>,
}

impl ExecControl for MockExecution {
    fn wait(&self) -> Result<ExecOutcome> {
        self.state.completion.wait()
    }

    fn cancel(&self) -> Result<()> {
        self.state.signal(|signals| signals.cancelled = true);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;

    use serde_json::Value;

    use super::*;
    use crate::input::InputCloser;
    use crate::model::StdinMode;
    use crate::stream;

    struct ObservedInput {
        input: InputSource,
        dropped: Arc<AtomicBool>,
    }

    impl Read for ObservedInput {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.input.read(buffer)
        }
    }

    impl Drop for ObservedInput {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }

    fn execution(input: InputSource, timeout: Option<Duration>) -> Box<dyn ExecControl> {
        let (stdout, _stdout) = stream::queue();
        echo_into(input, stdout, timeout)
    }

    fn echo_into(
        input: InputSource,
        stdout: stream::QueueWriter,
        timeout: Option<Duration>,
    ) -> Box<dyn ExecControl> {
        let backend = MockBackend::new().with_exec_handler(|_| MockExec {
            echo_stdin: true,
            ..MockExec::default()
        });
        let sandbox_id = backend
            .provision(&ProvisionRequest::new())
            .unwrap()
            .sandbox_id;
        backend.start(&sandbox_id).unwrap();
        let (stderr, _stderr) = stream::queue();
        let mut request = ExecRequest::command_line("cat").with_stdin(StdinMode::Piped);
        request.process.timeout = timeout;
        backend
            .exec(
                &sandbox_id,
                &request,
                ExecIo {
                    stdout: Box::new(stdout),
                    stderr: Box::new(stderr),
                    stdin: Some(input),
                },
            )
            .unwrap()
    }

    fn wait_with_deadline(control: Box<dyn ExecControl>) -> Result<ExecOutcome> {
        let (finished, received) = mpsc::channel();
        let worker = thread::spawn(move || finished.send(control.wait()).unwrap());
        let result = received.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
        result
    }

    #[test]
    fn terminal_outcomes_wait_for_the_input_worker_to_drop_its_reader() {
        use std::io::Write;

        for cancelled in [true, false] {
            let (reader, mut writer) = io::pipe().unwrap();
            let input = InputSource::from_pipe(reader).unwrap();
            let closer = input.close_handle();
            let dropped = Arc::new(AtomicBool::new(false));
            let input = InputSource::new(
                ObservedInput {
                    input,
                    dropped: Arc::clone(&dropped),
                },
                closer,
            );
            // A workload timeout could expire before the cancellation arrives, so only the timeout
            // case gets one; `wait_with_deadline` bounds the cancelled case.
            let control = execution(input, (!cancelled).then_some(Duration::from_millis(20)));
            if cancelled {
                control.cancel().unwrap();
            }
            assert_eq!(
                wait_with_deadline(control).unwrap(),
                if cancelled {
                    ExecOutcome::Cancelled
                } else {
                    ExecOutcome::TimedOut
                }
            );
            assert!(dropped.load(Ordering::Acquire));
            assert_eq!(
                writer.write(b"x").unwrap_err().kind(),
                io::ErrorKind::BrokenPipe
            );
        }
    }

    /// Endless standard input that reports once it has supplied more than an output queue holds.
    struct EndlessInput {
        supplied: usize,
        overflowed: Option<mpsc::Sender<()>>,
    }

    impl Read for EndlessInput {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.supplied += buffer.len();
            if self.supplied > stream::QUEUE_LIMIT
                && let Some(overflowed) = self.overflowed.take()
            {
                overflowed.send(()).unwrap();
            }
            Ok(buffer.len())
        }
    }

    #[test]
    fn terminal_outcomes_interrupt_an_echo_blocked_on_a_retained_full_stdout() {
        for cancelled in [true, false] {
            let (overflowed, overflow) = mpsc::channel();
            let input = InputSource::new(
                EndlessInput {
                    supplied: 0,
                    overflowed: Some(overflowed),
                },
                InputCloser::new(|| {}),
            );
            let (stdout, retained) = stream::queue();
            let timeout = (!cancelled).then_some(Duration::from_millis(500));
            let control = echo_into(input, stdout, timeout);
            // The echo has read more than the queue holds, so its pending write waits for a reader
            // that never drains.
            overflow.recv_timeout(Duration::from_secs(5)).unwrap();
            if cancelled {
                control.cancel().unwrap();
            }
            assert_eq!(
                wait_with_deadline(control).unwrap(),
                if cancelled {
                    ExecOutcome::Cancelled
                } else {
                    ExecOutcome::TimedOut
                }
            );
            // Output accepted before the interruption remains readable, followed by end-of-file.
            assert_eq!(retained.read_to_end_blocking().len(), stream::QUEUE_LIMIT);
        }
    }

    #[test]
    fn input_read_failures_are_reported_instead_of_treated_as_eof() {
        struct FailingInput;
        impl Read for FailingInput {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("injected input failure"))
            }
        }
        let input = InputSource::new(FailingInput, InputCloser::new(|| {}));
        let error = wait_with_deadline(execution(input, None)).unwrap_err();
        assert_eq!(error.code(), crate::ErrorCode::BackendError);
        assert_eq!(
            std::error::Error::source(&error).unwrap().to_string(),
            "injected input failure"
        );
    }

    fn disabled(path: &str, value: &Value, found: &mut Vec<String>) {
        match value {
            Value::Bool(false) => found.push(path.to_owned()),
            Value::Object(fields) => {
                for (name, field) in fields {
                    disabled(&format!("{path}.{name}"), field, found);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn full_capabilities_enable_every_feature() {
        let capabilities = serde_json::to_value(MockBackend::full_capabilities()).unwrap();
        let mut found = Vec::new();
        disabled("capabilities", &capabilities, &mut found);
        assert!(found.is_empty(), "the mock does not support {found:?}");
    }
}
