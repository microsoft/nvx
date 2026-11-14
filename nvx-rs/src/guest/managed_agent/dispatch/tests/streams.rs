//! NVXC version 2 through the shared loop: its requests, with `READY`'s version, and the
//! refusal of a frame of version 1; one execution at a time, with `BUSY` for another, and
//! `CANCEL` and `STOP` while it runs; its standard input, which the agent acknowledges once its
//! pipe took it, under the window, until its explicit end; its output past version 1's cap; and
//! the send queue, while which is full the agent reads no output and still serves control
//! requests.

use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use crate::guest::restore::GuestRestoreAction;
use crate::protocol::v2::{self, Frame, STDIN_WINDOW, VERSION};

use super::*;

/// A frame of version 2 that the agent sent, with its request ID.
#[derive(Debug, PartialEq, Eq)]
enum Reply {
    Ready(u64, Vec<u8>),
    Stdout(u64, Vec<u8>),
    Stderr(u64, Vec<u8>),
    Exit(u64, i32, v2::Outcome),
    Stopped(u64),
    Ack(u64, u32),
    Busy(u64),
    Error(u64, i32, Vec<u8>),
}

impl Harness {
    /// Sends a request of version 2. The agent's credit for it comes among its replies.
    fn send_v2(&mut self, request_id: u64, frame: Frame<'_>) {
        let frame = frame.encode(request_id).unwrap();
        let record = encode_outer(
            OUTER_DATA,
            &INSTANCE,
            self.epoch,
            self.host_sequence,
            &frame,
        );
        self.host.write_all(&record.unwrap()).unwrap();
        self.host_sequence += 1;
    }

    /// Reads the agent's next frame, of version 2, after the credits before it.
    fn next_v2(&mut self) -> Reply {
        loop {
            if let Some(reply) = self.next_record() {
                return reply;
            }
        }
    }

    /// Reads the agent's next frame, as [`next_v2`](Self::next_v2) does, unless none arrives
    /// within `timeout`.
    fn next_v2_within(&mut self, timeout: Duration) -> Option<Reply> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline
                .saturating_duration_since(Instant::now())
                .as_millis();
            let ready = sys::readable_or_hung_up(&[self.host.as_fd()], left as u16).unwrap();
            if !ready[0] {
                return None;
            }
            if let Some(reply) = self.next_record() {
                return Some(reply);
            }
        }
    }

    /// Reads the agent's next record: a credit, as `None`, or a frame of version 2.
    fn next_record(&mut self) -> Option<Reply> {
        let (header, payload) = self.read_outer();
        assert_eq!(
            (header.epoch, header.sequence),
            (self.epoch, self.guest_sequence)
        );
        self.guest_sequence += 1;
        if header.record_type == OUTER_CREDIT {
            return None;
        }
        assert_eq!(header.record_type, OUTER_DATA);
        let (id, frame) = Frame::decode(&payload).unwrap();
        Some(match frame {
            Frame::Ready {
                version,
                activation,
            } => {
                assert_eq!(version, VERSION);
                Reply::Ready(id, activation.to_vec())
            }
            Frame::Stdout(bytes) => Reply::Stdout(id, bytes.to_vec()),
            Frame::Stderr(bytes) => Reply::Stderr(id, bytes.to_vec()),
            Frame::Exit { status, outcome } => Reply::Exit(id, status, outcome),
            Frame::Stopped => Reply::Stopped(id),
            Frame::StdinAck(count) => Reply::Ack(id, count),
            Frame::Busy => Reply::Busy(id),
            Frame::Error { status, category } => Reply::Error(id, status, category.to_vec()),
            frame => panic!("the agent sent a request: {frame:?}"),
        })
    }

    /// Reads the output of execution `id` up to its first line, which its shell's `echo $$`
    /// wrote, and returns that process, and what followed that line in the same reads.
    fn shell_process(&mut self, id: u64) -> (sys::ProcessId, Vec<u8>) {
        let mut output = Vec::new();
        let end = loop {
            if let Some(end) = output.iter().position(|&byte| byte == b'\n') {
                break end;
            }
            let Reply::Stdout(reply, bytes) = self.next_v2() else {
                panic!("the shell did not report its process ID");
            };
            assert_eq!(reply, id);
            output.extend(bytes);
        };
        let rest = output.split_off(end + 1);
        let process = std::str::from_utf8(&output).unwrap().trim_end().parse();
        (
            sys::ProcessId::from_positive(process.unwrap()).unwrap(),
            rest,
        )
    }

    /// Stops the agent over version 2, and returns its system.
    fn stop_v2(mut self, request_id: u64) -> TestSystem {
        self.send_v2(request_id, Frame::Stop);
        assert_eq!(self.next_v2(), Reply::Stopped(request_id));
        let (outcome, system) = self.thread.join().unwrap();
        assert!(matches!(outcome, Ok(Outcome::Stopped)));
        system
    }
}

/// Waits until `process` has ended and its status was collected, or fails the test.
fn until_gone(process: sys::ProcessId) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while sys::process_exists(process) {
        assert!(Instant::now() < deadline, "the workload did not end");
        thread::sleep(Duration::from_millis(5));
    }
}

/// Returns the payload of an `EXEC` request for `script` in `/bin/sh`.
fn shell(script: &str) -> Vec<u8> {
    workload_payload(&["/bin/sh", "-c", script], None).unwrap()
}

#[test]
fn version_2_serves_ping_and_the_mapping_table() {
    let (temporary, system) = TestSystem::with_host_export();
    let export = system.paths.hostfs_dir.join("root").canonicalize().unwrap();
    let source = temporary.path().canonicalize().unwrap().join("guest/src");
    let maps = maps_payload(
        0,
        &[(
            b"0/src",
            source.as_os_str().as_bytes(),
            protocol::MAP_READ_ONLY,
        )],
        None,
    );
    let mut harness = Harness::with_system(system, 1);
    harness.send_v2(1, Frame::Ping);
    assert_eq!(harness.next_v2(), Reply::Ready(1, Vec::new()));
    harness.send_v2(2, Frame::Exec(&exec_payload(None)));
    assert_eq!(
        harness.next_v2(),
        Reply::Error(2, Errno::EAGAIN as i32, b"mappings-incomplete".to_vec())
    );
    harness.send_v2(3, Frame::Maps(&maps));
    assert_eq!(harness.next_v2(), Reply::Ready(3, Vec::new()));
    let system = harness.stop_v2(4);
    assert_eq!(
        system.kernel.calls(),
        attach_calls(&export.join("0/src"), &source, Access::ReadOnly)
    );
    assert_eq!(system.tty_drained, 1);
    assert!(system.diagnostics.is_empty());
}

/// A request of version 1, as its conformance vector holds it, ends the session as a malformed
/// request does.
#[test]
fn a_request_of_version_1_ends_the_session() {
    let frame = crate::tests::vector(crate::tests::NVXC_V1, "FRAME", "PING").bytes;
    let diagnostics =
        Harness::new().fail(&encode_outer(OUTER_DATA, &INSTANCE, 1, 8, &frame).unwrap());
    let line =
        |stage: &str, status: i32| format!("NVX-MANAGED-ERROR: stage={stage} status={status}\n");
    assert_eq!(
        diagnostics,
        [
            line("data-application", 24),
            line("data", Errno::EPROTO as i32)
        ]
        .concat()
        .as_bytes()
    );
}

#[test]
fn version_2_has_no_features_request() {
    let frame = [&b"NVXC\x02\x05\0\0"[..], &42u64.to_le_bytes(), &[0; 8]].concat();
    let diagnostics =
        Harness::new().fail(&encode_outer(OUTER_DATA, &INSTANCE, 1, 8, &frame).unwrap());
    let line =
        |stage: &str, status: i32| format!("NVX-MANAGED-ERROR: stage={stage} status={status}\n");
    assert_eq!(
        diagnostics,
        [
            line("data-application", 24),
            line("data", Errno::EPROTO as i32)
        ]
        .concat()
        .as_bytes()
    );
}

#[test]
fn version_2_carries_capture_and_activation_with_their_payloads() {
    let config = AgentConfig::for_test();
    let mappings = HostMappings::announced(0);
    let system = TestSystem::new(Paths::default());
    let mut harness =
        Harness::spawn_with_capture(system, config, mappings, Some(Capture::fixture(true)));
    harness.reset(1, 7);
    harness.send_v2(1, Frame::Ping);
    assert_eq!(
        harness.next_v2(),
        Reply::Error(1, Errno::ENOTSUP as i32, b"unsupported-operation".to_vec())
    );
    // Outside PID 1, the capture's preparation fails, which the agent reports as version 1
    // did.
    harness.send_v2(2, Frame::Capture(&[0]));
    assert_eq!(
        harness.next_v2(),
        Reply::Error(2, Errno::EPERM as i32, b"capture-failed".to_vec())
    );
    assert!(harness.stop_v2(3).diagnostics.is_empty());

    // A restored session's activate request gets the READY of version 2, with what Ready
    // reports about the activation.
    let config = AgentConfig::for_test();
    let mappings = HostMappings::announced(0);
    let (mut capture, payload) = Capture::activation_fixture();
    let restored = capture.restored.as_mut().unwrap();
    let now = Instant::now();
    assert!(matches!(
        restored.accept(&payload, now).unwrap().0,
        GuestRestoreAction::Apply(_)
    ));
    let ready = restored.activation.complete(now, Ok(())).unwrap().encode();
    let system = TestSystem::new(Paths::default());
    let mut harness = Harness::spawn_with_capture(system, config, mappings, Some(capture));
    harness.reset(1, 7);
    harness.send_v2(4, Frame::Activate(&payload));
    assert_eq!(harness.next_v2(), Reply::Ready(4, ready));
    assert!(harness.stop_v2(5).diagnostics.is_empty());
}

#[test]
fn a_version_2_execution_takes_its_input_until_its_end_and_acknowledges_what_it_wrote() -> TestResult
{
    if super::super::super::tests::isolate_process_test(
        "NVX_V2_INPUT_TEST_CHILD",
        "guest::managed_agent::dispatch::tests::streams::a_version_2_execution_takes_its_input_until_its_end_and_acknowledges_what_it_wrote",
    ) {
        return Ok(());
    }
    let _process = super::super::super::tests::PROCESS_TEST_LOCK.lock()?;
    let (_temporary, mut harness) = direct_harness();
    harness.send_v2(42, Frame::Exec(&workload_payload(&["/bin/cat"], None)?));
    let input: Vec<u8> = (0..200_000u32).map(|index| (index % 251) as u8).collect();
    let (mut sent, mut acknowledged, mut ended) = (0, 0, false);
    let mut output = Vec::new();
    let exit = loop {
        // The host never has more than the window unacknowledged.
        let room = STDIN_WINDOW as usize - (sent - acknowledged);
        let chunk = room.min(16 * 1024).min(input.len() - sent);
        if chunk != 0 {
            harness.send_v2(42, Frame::Stdin(&input[sent..sent + chunk]));
            sent += chunk;
            continue;
        }
        if sent == input.len() && !ended {
            harness.send_v2(42, Frame::StdinEnd);
            ended = true;
        }
        match harness.next_v2() {
            Reply::Ack(42, count) => {
                acknowledged += count as usize;
                assert!(acknowledged <= sent);
            }
            Reply::Stdout(42, bytes) => output.extend(bytes),
            Reply::Exit(42, status, outcome) => break (status, outcome),
            reply => panic!("unexpected {reply:?}"),
        }
    };
    // `cat` ends only once its input ends, after every byte that the agent wrote into its pipe.
    assert_eq!(exit, (0, v2::Outcome::Exit));
    assert_eq!(output, input);
    assert_eq!(acknowledged, input.len());
    // Input of an execution that ended takes no more bytes, and gets no answer.
    harness.send_v2(42, Frame::Stdin(b"late"));
    harness.send_v2(42, Frame::StdinEnd);
    harness.send_v2(43, Frame::Ping);
    assert_eq!(harness.next_v2(), Reply::Ready(43, Vec::new()));
    assert!(harness.stop_v2(44).diagnostics.is_empty());
    Ok(())
}

#[test]
fn a_version_2_execution_never_has_more_than_the_window_unacknowledged() -> TestResult {
    if super::super::super::tests::isolate_process_test(
        "NVX_V2_WINDOW_TEST_CHILD",
        "guest::managed_agent::dispatch::tests::streams::a_version_2_execution_never_has_more_than_the_window_unacknowledged",
    ) {
        return Ok(());
    }
    let _process = super::super::super::tests::PROCESS_TEST_LOCK.lock()?;
    let (_temporary, mut harness) = direct_harness();
    // The workload never reads its input, so its pipe takes what it holds, and no more.
    harness.send_v2(42, Frame::Exec(&shell("echo $$; exec sleep 30")));
    let (process, _) = harness.shell_process(42);
    let window = STDIN_WINDOW as usize;
    let chunk = [0x5a; 16 * 1024];
    let (mut sent, mut acknowledged) = (0, 0);
    loop {
        while sent - acknowledged < window {
            let length = (window - (sent - acknowledged)).min(chunk.len());
            harness.send_v2(42, Frame::Stdin(&chunk[..length]));
            sent += length;
        }
        match harness.next_v2_within(Duration::from_millis(500)) {
            Some(Reply::Ack(42, count)) => acknowledged += count as usize,
            None => break,
            Some(reply) => panic!("unexpected {reply:?}"),
        }
    }
    // The agent acknowledged what the pipe took, and holds the rest of a full window.
    assert!(acknowledged > 0);
    assert_eq!(sent - acknowledged, window);
    assert!(sys::process_exists(process));
    // A byte beyond the window ends the session, and the workload with it.
    harness.send_v2(42, Frame::Stdin(b"x"));
    let (outcome, system) = harness.thread.join().unwrap();
    assert_eq!(outcome.err(), Some(Errno::EPROTO));
    assert_eq!(
        system.diagnostics,
        format!(
            "NVX-MANAGED-ERROR: stage=data status={}\n",
            Errno::EPROTO as i32
        )
        .as_bytes()
    );
    until_gone(process);
    Ok(())
}

#[test]
fn a_version_2_execution_runs_alone_and_ends_on_cancel_or_stop() -> TestResult {
    if super::super::super::tests::isolate_process_test(
        "NVX_V2_BUSY_TEST_CHILD",
        "guest::managed_agent::dispatch::tests::streams::a_version_2_execution_runs_alone_and_ends_on_cancel_or_stop",
    ) {
        return Ok(());
    }
    let _process = super::super::super::tests::PROCESS_TEST_LOCK.lock()?;
    let (_temporary, mut harness) = direct_harness();
    let sleeper = shell("echo $$; exec sleep 30");
    harness.send_v2(42, Frame::Exec(&sleeper));
    let (process, _) = harness.shell_process(42);
    harness.send_v2(43, Frame::Exec(&sleeper));
    assert_eq!(harness.next_v2(), Reply::Busy(43));
    harness.send_v2(44, Frame::Ping);
    assert_eq!(harness.next_v2(), Reply::Busy(44));
    // A cancellation of another request leaves the execution running.
    harness.send_v2(41, Frame::Cancel);
    harness.send_v2(42, Frame::Cancel);
    assert_eq!(
        harness.next_v2(),
        Reply::Exit(42, 128 + 9, v2::Outcome::Cancelled)
    );
    until_gone(process);
    // `STOP` ends the running execution, which reports it, and then the session.
    harness.send_v2(45, Frame::Exec(&sleeper));
    let (process, _) = harness.shell_process(45);
    harness.send_v2(46, Frame::Stop);
    assert_eq!(
        harness.next_v2(),
        Reply::Exit(45, 128 + 9, v2::Outcome::Cancelled)
    );
    assert_eq!(harness.next_v2(), Reply::Stopped(46));
    let (outcome, system) = harness.thread.join().unwrap();
    assert!(matches!(outcome, Ok(Outcome::Stopped)));
    assert_eq!(system.tty_drained, 1);
    assert!(system.diagnostics.is_empty());
    until_gone(process);
    Ok(())
}

#[test]
fn a_version_2_execution_forwards_output_past_the_old_cap() -> TestResult {
    if super::super::super::tests::isolate_process_test(
        "NVX_V2_OUTPUT_TEST_CHILD",
        "guest::managed_agent::dispatch::tests::streams::a_version_2_execution_forwards_output_past_the_old_cap",
    ) {
        return Ok(());
    }
    let _process = super::super::super::tests::PROCESS_TEST_LOCK.lock()?;
    let (_temporary, mut harness) = direct_harness();
    let total = 3 * 1024 * 1024 / 2;
    harness.send_v2(
        42,
        Frame::Exec(&shell(&format!("head -c {total} /dev/zero"))),
    );
    let mut forwarded = 0;
    loop {
        match harness.next_v2() {
            Reply::Stdout(42, bytes) => {
                assert!(bytes.iter().all(|&byte| byte == 0));
                forwarded += bytes.len();
            }
            Reply::Exit(42, 0, v2::Outcome::Exit) => break,
            reply => panic!("unexpected {reply:?}"),
        }
    }
    assert_eq!(forwarded, total);
    assert!(harness.stop_v2(43).diagnostics.is_empty());
    Ok(())
}

#[test]
fn the_agent_reads_no_output_while_its_send_queue_is_full() -> TestResult {
    if super::super::super::tests::isolate_process_test(
        "NVX_V2_BACKPRESSURE_TEST_CHILD",
        "guest::managed_agent::dispatch::tests::streams::the_agent_reads_no_output_while_its_send_queue_is_full",
    ) {
        return Ok(());
    }
    let _process = super::super::super::tests::PROCESS_TEST_LOCK.lock()?;
    let (temporary, mut harness) = direct_harness();
    let marker = temporary.path().join("finished");
    let lines = 1_000_000;
    let script = format!("echo $$; seq 1 {lines}; touch '{}'", marker.display());
    harness.send_v2(42, Frame::Exec(&shell(&script)));
    let (process, mut output) = harness.shell_process(42);
    // The host reads nothing: the console, then the send queue, then the workload's pipe fill,
    // and the workload waits to write the rest of its output.
    thread::sleep(Duration::from_secs(1));
    assert!(sys::process_exists(process));
    assert!(!marker.exists());
    // The agent still serves control requests, so the cancellation ends the workload.
    harness.send_v2(42, Frame::Cancel);
    until_gone(process);
    // What it forwarded then arrives, in order, before the outcome.
    let exit = loop {
        match harness.next_v2() {
            Reply::Stdout(42, bytes) => output.extend(bytes),
            Reply::Exit(42, status, outcome) => break (status, outcome),
            reply => panic!("unexpected {reply:?}"),
        }
    };
    assert_eq!(exit, (128 + 9, v2::Outcome::Cancelled));
    let expected: String = (1..=lines).map(|line| format!("{line}\n")).collect();
    assert!(output.len() < expected.len());
    assert!(expected.as_bytes().starts_with(&output));
    assert!(!marker.exists());
    assert!(harness.stop_v2(43).diagnostics.is_empty());
    Ok(())
}
