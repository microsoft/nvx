//! The encodings of the execution messages: round trips, the layout of a start, the bodies that
//! break the rules, and a peer and a runner that exchange their messages through the frames; and
//! the host's side of an execution, which sends the cancellation of one that it gives up on at
//! once, and cancels one whose stream outgrows what it keeps.

use std::time::{Duration, Instant};

use nvx::exec::{
    Cause, Key, LaunchFailure, Limits, Outcome, Peer, Pool, Runner, Settlement, Status, Stream,
    Timers, Window, peer, runner,
};

use super::{
    AccountMode, GuestMessage, HostExecution, HostMessage, Identity, KEY_LEN, MAX_CAPTURE,
    MAX_CHUNK, Start, name,
};
use crate::frame::{MAX_BODY, Message};

/// The key of most executions in these tests.
const KEY: Key = Key::new(0x0102_0304_0506_0708, 9);

/// Returns limits with chunks of 16 KiB, an input window of 4 frames, and one output pool of 8
/// frames that both streams share.
fn shared() -> Limits {
    Limits::new(16 * 1024, Window::frames(4), Window::frames(8))
        .unwrap()
        .shared_output()
}

/// Returns timers with a grace of a second, a settlement time of five, and no timeout.
fn timers() -> Timers {
    Timers::new(Duration::from_secs(1), Duration::from_secs(5))
}

/// Returns an identity with every field: supplementary groups, capabilities, and numeric IDs.
fn numeric() -> Identity {
    Identity {
        groups: vec![0, 50, u32::MAX],
        capabilities: 1 | 1 << 13 | 1 << 63,
        account: AccountMode::Numeric,
        ..Identity::new(4242, u32::MAX)
    }
}

/// Returns starts with every unit, flag, and kind of argument, of identity, of working
/// directory, and of environment.
fn starts() -> Vec<Start> {
    let bytes = Limits::new(1, Window::bytes(1), Window::bytes(u32::MAX)).unwrap();
    let largest = Limits::new(MAX_CHUNK, Window::frames(u32::MAX), Window::frames(1)).unwrap();
    vec![
        Start::new(KEY, shared(), timers(), ["/bin/sh", "-c", "seq 1 10"]),
        Start::new(KEY, shared(), timers(), ["/bin/cat"]).with_input(),
        Start::new(
            Key::new(u64::MAX, u64::MAX),
            bytes,
            Timers::new(Duration::ZERO, Duration::from_millis(u64::MAX)).timeout(Duration::ZERO),
            [b"/x".to_vec(), Vec::new(), (1..=u8::MAX).collect()],
        )
        .with_identity(numeric())
        .with_working_directory("/")
        .with_environment(["A=", "B==", "PATH=/usr/bin:/bin"]),
        Start::new(
            Key::new(0, 0),
            largest,
            timers().timeout(Duration::from_millis(1500)),
            ["/bin/true"],
        )
        .with_input()
        .with_identity(Identity::new(0, 0))
        .with_working_directory(std::iter::once(b'/').chain(1..=u8::MAX).collect::<Vec<_>>())
        .with_environment([(1..=u8::MAX).collect::<Vec<_>>()]),
    ]
}

#[test]
fn starts_round_trip_with_every_unit_flag_and_argument() {
    for start in starts() {
        let body = start.encode();
        assert_eq!(Start::decode(&body), Ok(start.clone()));
        let message = HostMessage::Start(start.clone()).encode();
        assert_eq!(message, Message::ExecStart(body));
        assert_eq!(message.encode().unwrap()[4], 12);
        assert_eq!(
            HostMessage::decode(&message),
            Some(Ok(HostMessage::Start(start)))
        );
    }
}

#[test]
fn a_start_s_layout_is_its_key_limits_timers_input_and_arguments() {
    let start = Start::new(
        Key::new(1, 2),
        Limits::new(16, Window::bytes(32), Window::frames(4))
            .unwrap()
            .shared_output(),
        Timers::new(Duration::from_millis(5), Duration::from_millis(6))
            .timeout(Duration::from_millis(7)),
        ["/p", "a"],
    )
    .with_input();
    let mut expected = Vec::new();
    expected.extend_from_slice(&1u64.to_le_bytes());
    expected.extend_from_slice(&2u64.to_le_bytes());
    expected.extend_from_slice(&16u32.to_le_bytes());
    expected.push(0);
    expected.extend_from_slice(&32u32.to_le_bytes());
    expected.push(1);
    expected.extend_from_slice(&4u32.to_le_bytes());
    expected.push(1);
    for milliseconds in [5u64, 6] {
        expected.extend_from_slice(&milliseconds.to_le_bytes());
    }
    expected.push(1);
    expected.extend_from_slice(&7u64.to_le_bytes());
    expected.push(1);
    expected.extend_from_slice(&2u16.to_le_bytes());
    expected.extend_from_slice(&2u32.to_le_bytes());
    expected.extend_from_slice(b"/p");
    expected.extend_from_slice(&1u32.to_le_bytes());
    expected.extend_from_slice(b"a");
    // No identity, no working directory, and an empty environment.
    expected.extend_from_slice(&[0, 0, 0, 0]);
    assert_eq!(start.encode(), expected);
}

#[test]
fn a_start_s_layout_ends_with_its_identity_working_directory_and_environment() {
    let start = Start::new(KEY, shared(), timers(), ["/p"])
        .with_identity(Identity {
            groups: vec![50, 60],
            capabilities: 1 << 13 | 1,
            account: AccountMode::Numeric,
            ..Identity::new(1000, 100)
        })
        .with_working_directory("/w")
        .with_environment(["A=1", "B="]);
    let head = Start::new(KEY, shared(), timers(), ["/p"]).encode();
    let mut expected = head[..head.len() - 4].to_vec();
    expected.push(1);
    expected.extend_from_slice(&1000u32.to_le_bytes());
    expected.extend_from_slice(&100u32.to_le_bytes());
    expected.push(1);
    expected.extend_from_slice(&0x2001u64.to_le_bytes());
    expected.extend_from_slice(&2u16.to_le_bytes());
    expected.extend_from_slice(&50u32.to_le_bytes());
    expected.extend_from_slice(&60u32.to_le_bytes());
    expected.push(1);
    expected.extend_from_slice(&2u32.to_le_bytes());
    expected.extend_from_slice(b"/w");
    expected.extend_from_slice(&2u16.to_le_bytes());
    expected.extend_from_slice(&3u32.to_le_bytes());
    expected.extend_from_slice(b"A=1");
    expected.extend_from_slice(&2u32.to_le_bytes());
    expected.extend_from_slice(b"B=");
    assert_eq!(start.encode(), expected);
    // An existing account is 0.
    let existing = Start::new(KEY, shared(), timers(), ["/p"]).with_identity(Identity::new(0, 0));
    assert_eq!(existing.encode()[head.len() - 4 + 9], 0);
}

#[test]
fn starts_that_break_the_rules_are_refused() {
    let valid = Start::new(KEY, shared(), timers(), ["/bin/sh", "-c", "exit"]).encode();
    let full = starts()[2].encode();
    // Every start that ends early, and one with a byte too many.
    for body in [&valid, &full] {
        for length in 0..body.len() {
            let problem = Start::decode(&body[..length]).unwrap_err();
            assert_eq!(problem.problem(), "an execution's start ends early");
        }
    }
    let mut longer = valid.clone();
    longer.push(0);
    assert_eq!(
        Start::decode(&longer).unwrap_err().problem(),
        "an execution's start has 1 byte too many"
    );
    // The fields at their offsets after the key: the chunk at 16, the input's unit and size at
    // 20 and 21, the output's at 25 and 26, the sharing at 30, the timeout's flag and value at
    // 47 and 48, the input's flag at 56, and the argument count at 57.
    let changed = |offset: usize, bytes: &[u8]| {
        let mut body = valid.clone();
        body[offset..offset + bytes.len()].copy_from_slice(bytes);
        Start::decode(&body).unwrap_err().problem().to_owned()
    };
    let refused = |problem: &str| format!("an execution's start {problem}");
    assert_eq!(
        changed(16, &(MAX_CHUNK + 1).to_le_bytes()),
        refused(&format!(
            "has a chunk of {} bytes, more than {MAX_CHUNK}",
            MAX_CHUNK + 1
        ))
    );
    assert_eq!(
        changed(16, &0u32.to_le_bytes()),
        refused("has an empty chunk or window")
    );
    assert_eq!(
        changed(21, &0u32.to_le_bytes()),
        refused("has an empty chunk or window")
    );
    assert_eq!(
        changed(26, &0u32.to_le_bytes()),
        refused("has an empty chunk or window")
    );
    assert_eq!(changed(20, &[2]), refused("names the unknown unit 2"));
    assert_eq!(changed(25, &[7]), refused("names the unknown unit 7"));
    for offset in [30, 47, 56] {
        assert_eq!(changed(offset, &[2]), refused("has the flag 2"), "{offset}");
    }
    assert_eq!(
        changed(48, &1u64.to_le_bytes()),
        refused("has a timeout without its flag")
    );
    // Arguments: none, an empty program, a NUL byte, and a length beyond the body.
    let mut none = valid[..57].to_vec();
    none.extend_from_slice(&0u16.to_le_bytes());
    none.extend_from_slice(&[0, 0, 0, 0]);
    assert_eq!(
        Start::decode(&none).unwrap_err().problem(),
        refused("names no program")
    );
    let empty = Start::new(KEY, shared(), timers(), [""]).encode();
    assert_eq!(
        Start::decode(&empty).unwrap_err().problem(),
        refused("names an empty program")
    );
    let nul = Start::new(KEY, shared(), timers(), ["/bin/sh", "a\0b"]).encode();
    assert_eq!(
        Start::decode(&nul).unwrap_err().problem(),
        refused("has an argument with a NUL byte")
    );
    let mut beyond = valid[..59].to_vec();
    beyond.extend_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        Start::decode(&beyond).unwrap_err().problem(),
        "an execution's start ends early"
    );
    // The flags of the identity and the working directory, and the account mode, which the
    // identity's IDs precede.
    let head = &valid[..valid.len() - 4];
    let ending = |tail: &[u8]| {
        let body = [head, tail].concat();
        Start::decode(&body).unwrap_err().problem().to_owned()
    };
    assert_eq!(ending(&[2, 0, 0, 0]), refused("has the flag 2"));
    assert_eq!(ending(&[0, 3, 0, 0]), refused("has the flag 3"));
    let mut identity = vec![1];
    identity.extend_from_slice(&[0; 8]);
    identity.push(2);
    identity.extend_from_slice(&[0; 10]);
    identity.extend_from_slice(&[0, 0, 0]);
    assert_eq!(
        ending(&identity),
        refused("names the unknown account mode 2")
    );
    // Working directories that are not absolute, or hold a NUL byte, and environment entries
    // that are not `NAME=VALUE`, or hold a NUL byte.
    let start = || Start::new(KEY, shared(), timers(), ["/bin/true"]);
    let cases = [
        (
            start().with_working_directory("relative"),
            "names a working directory that is not absolute",
        ),
        (
            start().with_working_directory(""),
            "names a working directory that is not absolute",
        ),
        (
            start().with_working_directory("/a\0b"),
            "has a working directory with a NUL byte",
        ),
        (
            start().with_environment(["A=1", "NAME"]),
            "has an environment entry that is not NAME=VALUE",
        ),
        (
            start().with_environment(["=value"]),
            "has an environment entry that is not NAME=VALUE",
        ),
        (
            start().with_environment([""]),
            "has an environment entry that is not NAME=VALUE",
        ),
        (
            start().with_environment(["A=b\0c"]),
            "has an environment entry with a NUL byte",
        ),
    ];
    for (start, problem) in cases {
        assert_eq!(
            Start::decode(&start.encode()).unwrap_err().problem(),
            refused(problem),
            "{start:?}"
        );
    }
}

/// Returns outcomes with every status, cause, launch failure, and flag.
fn outcomes() -> Vec<Outcome> {
    let mut outcomes = vec![
        Outcome::new(None),
        Outcome::new(Some(Status::Exited(0))).with_reusable(),
        Outcome::new(Some(Status::Exited(-1))),
        Outcome::new(Some(Status::Exited(i32::MAX))).with_truncated(Stream::Stdout),
        Outcome::new(Some(Status::Signaled(9))).with_truncated(Stream::Stderr),
        Outcome::new(Some(Status::Exited(125)))
            .with_launch_failure(LaunchFailure::new(8, 2))
            .with_reusable(),
        Outcome::new(None).with_launch_failure(LaunchFailure::new(u32::MAX, i32::MIN)),
        Outcome::new(Some(Status::Signaled(i32::MIN)))
            .with_truncated(Stream::Stdout)
            .with_truncated(Stream::Stderr),
    ];
    for cause in [
        Cause::Cancelled,
        Cause::Dropped,
        Cause::TimedOut,
        Cause::Reset,
        Cause::Disconnected,
    ] {
        outcomes.push(Outcome::new(Some(Status::Signaled(9))).with_cause(cause));
    }
    outcomes
}

#[test]
fn host_messages_round_trip_through_their_frames() {
    let bytes = (0..=u8::MAX).collect::<Vec<_>>();
    let messages = [
        (HostMessage::Input(KEY, b"x"), 13),
        (HostMessage::Input(KEY, &bytes), 13),
        (HostMessage::InputEnd(KEY), 14),
        (HostMessage::Cancel(KEY, Cause::Cancelled), 15),
        (HostMessage::Cancel(KEY, Cause::Dropped), 15),
        (HostMessage::Cancel(KEY, Cause::TimedOut), 15),
        (HostMessage::Credit(KEY, Pool::Stdout, 1), 16),
        (HostMessage::Credit(KEY, Pool::Stderr, u32::MAX), 16),
        (HostMessage::Credit(KEY, Pool::Output, 8), 16),
    ];
    for (message, kind) in messages {
        let frame = message.encode();
        assert_eq!(frame.encode().unwrap()[4], kind, "{message:?}");
        assert_eq!(HostMessage::decode(&frame), Some(Ok(message.clone())));
        assert_eq!(message.key(), KEY);
    }
}

#[test]
fn guest_messages_round_trip_through_their_frames() {
    let bytes = (0..=u8::MAX).rev().collect::<Vec<_>>();
    let mut messages = vec![
        (GuestMessage::Credit(KEY, 1), 16),
        (GuestMessage::Credit(KEY, u32::MAX), 16),
        (GuestMessage::Started(KEY), 17),
        (GuestMessage::Output(KEY, Stream::Stdout, b"x"), 18),
        (GuestMessage::Output(KEY, Stream::Stderr, &bytes), 18),
        (GuestMessage::End(KEY, Stream::Stdout), 19),
        (GuestMessage::End(KEY, Stream::Stderr), 19),
        (GuestMessage::InputClosed(KEY), 20),
    ];
    for outcome in outcomes() {
        messages.push((GuestMessage::Terminal(KEY, outcome), 21));
    }
    for (message, kind) in messages {
        let frame = message.encode();
        assert_eq!(frame.encode().unwrap()[4], kind, "{message:?}");
        assert_eq!(GuestMessage::decode(&frame), Some(Ok(message)));
        assert_eq!(message.key(), KEY);
    }
}

#[test]
fn the_longest_chunk_fits_the_framing() {
    let chunk = vec![7u8; MAX_CHUNK as usize];
    let input = HostMessage::Input(KEY, &chunk).encode();
    let output = GuestMessage::Output(KEY, Stream::Stderr, &chunk).encode();
    assert_eq!(output.encode().unwrap().len(), 5 + MAX_BODY);
    assert!(input.encode().is_ok());
    let longer = vec![7u8; MAX_CHUNK as usize + 1];
    assert!(
        GuestMessage::Output(KEY, Stream::Stdout, &longer)
            .encode()
            .encode()
            .is_err()
    );
}

/// Returns `key`'s body, then `rest`.
fn body(key: Key, rest: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&key.id().to_le_bytes());
    body.extend_from_slice(&key.generation().to_le_bytes());
    body.extend_from_slice(rest);
    body
}

/// Returns the problem with which the host's or the guest's decoder refuses `message`.
fn refusal(message: &Message) -> String {
    let error = HostMessage::decode(message)
        .map(|decoded| decoded.unwrap_err())
        .or_else(|| GuestMessage::decode(message).map(|decoded| decoded.unwrap_err()))
        .expect("an execution message");
    error.problem().to_owned()
}

#[test]
fn bodies_that_break_the_rules_are_refused() {
    let key = name(KEY);
    let cases = [
        (
            Message::ExecInput(body(KEY, b"")),
            format!("the input of execution {key} has no byte"),
        ),
        (
            Message::ExecInput(vec![0; KEY_LEN - 1]),
            "an execution's input ends early".to_owned(),
        ),
        (
            Message::ExecInputEnd(body(KEY, b"x")),
            "an end of input has 1 byte too many".to_owned(),
        ),
        (
            Message::ExecInputEnd(Vec::new()),
            "an end of input ends early".to_owned(),
        ),
        (
            Message::ExecCancel(body(KEY, &[0])),
            format!("the cancellation of execution {key} names the cause 0"),
        ),
        (
            Message::ExecCancel(body(KEY, &[4])),
            format!("the cancellation of execution {key} names the cause 4"),
        ),
        (
            Message::ExecCancel(body(KEY, &[5])),
            format!("the cancellation of execution {key} names the cause 5"),
        ),
        (
            Message::ExecCancel(body(KEY, &[0xff])),
            format!("the cancellation of execution {key} names the cause 255"),
        ),
        (
            Message::ExecCancel(body(KEY, &[1, 1])),
            "an execution's cancellation has 1 byte too many".to_owned(),
        ),
        (
            Message::ExecCredit(body(KEY, &[4, 1, 0, 0, 0])),
            format!("the credit of execution {key} names the unknown pool 4"),
        ),
        (
            Message::ExecCredit(body(KEY, &[1, 0, 0, 0, 0])),
            format!("the credit of execution {key} has no unit"),
        ),
        (
            Message::ExecCredit(body(KEY, &[1, 1, 0, 0])),
            "an execution's credit ends early".to_owned(),
        ),
        (
            Message::ExecStarted(body(KEY, b"x")),
            "a start's confirmation has 1 byte too many".to_owned(),
        ),
        (
            Message::ExecOutput(body(KEY, &[0, b'x'])),
            format!("a message of execution {key} names the unknown stream 0"),
        ),
        (
            Message::ExecOutput(body(KEY, &[3, b'x'])),
            format!("a message of execution {key} names the unknown stream 3"),
        ),
        (
            Message::ExecOutput(body(KEY, &[1])),
            format!("the output of execution {key} has no byte"),
        ),
        (
            Message::ExecEnd(body(KEY, &[2, 0])),
            "a stream's end has 1 byte too many".to_owned(),
        ),
        (
            Message::ExecEnd(body(KEY, &[])),
            "a stream's end ends early".to_owned(),
        ),
        (
            Message::ExecInputClosed(body(KEY, &[0])),
            "an input's close has 1 byte too many".to_owned(),
        ),
    ];
    for (message, problem) in cases {
        assert_eq!(refusal(&message), problem, "{message:?}");
    }
    // The host credits only output pools, and the guest only the input.
    assert_eq!(
        HostMessage::decode(&Message::ExecCredit(body(KEY, &[0, 1, 0, 0, 0])))
            .unwrap()
            .unwrap_err()
            .problem(),
        format!("the host credits the input of execution {key}")
    );
    for (code, pool) in [(1, "standard output"), (2, "standard error"), (3, "output")] {
        assert_eq!(
            GuestMessage::decode(&Message::ExecCredit(body(KEY, &[code, 1, 0, 0, 0])))
                .unwrap()
                .unwrap_err()
                .problem(),
            format!("the guest credits the {pool} pool of execution {key}")
        );
    }
    // Outcomes: each field that breaks the rules, and every outcome that ends early.
    let valid = GuestMessage::Terminal(KEY, Outcome::new(Some(Status::Exited(0)))).encode();
    let Message::ExecTerminal(valid) = valid else {
        panic!("{valid:?}");
    };
    let changed = |offset: usize, bytes: &[u8]| {
        let mut body = valid.clone();
        body[offset..offset + bytes.len()].copy_from_slice(bytes);
        refusal(&Message::ExecTerminal(body))
    };
    let refused = |problem: &str| format!("the outcome of execution {key} {problem}");
    assert_eq!(changed(16, &[3]), refused("has the status 3 0"));
    assert_eq!(changed(16, &[0, 1, 0, 0, 0]), refused("has the status 0 1"));
    assert_eq!(changed(21, &[6]), refused("names the unknown cause 6"));
    assert_eq!(changed(22, &[2]), refused("has the launch failure 2 0 0"));
    assert_eq!(changed(23, &[1]), refused("has the launch failure 0 1 0"));
    assert_eq!(changed(31, &[8]), refused("has the flags 0x8"));
    for length in 0..valid.len() {
        assert_eq!(
            refusal(&Message::ExecTerminal(valid[..length].to_vec())),
            "an execution's outcome ends early"
        );
    }
    let mut longer = valid.clone();
    longer.push(0);
    assert_eq!(
        refusal(&Message::ExecTerminal(longer)),
        "an execution's outcome has 1 byte too many"
    );
}

#[test]
fn each_side_decodes_only_its_own_messages() {
    let host = [
        HostMessage::Start(Start::new(KEY, shared(), timers(), ["/bin/true"])).encode(),
        HostMessage::Input(KEY, b"x").encode(),
        HostMessage::InputEnd(KEY).encode(),
        HostMessage::Cancel(KEY, Cause::Cancelled).encode(),
    ];
    for message in &host {
        assert_eq!(GuestMessage::decode(message), None, "{message:?}");
    }
    let guest = [
        GuestMessage::Started(KEY).encode(),
        GuestMessage::Output(KEY, Stream::Stdout, b"x").encode(),
        GuestMessage::End(KEY, Stream::Stdout).encode(),
        GuestMessage::InputClosed(KEY).encode(),
        GuestMessage::Terminal(KEY, Outcome::new(None)).encode(),
    ];
    for message in &guest {
        assert_eq!(HostMessage::decode(message), None, "{message:?}");
    }
    for message in [Message::Ping, Message::Echo(b"x".to_vec()), Message::Stop] {
        assert_eq!(HostMessage::decode(&message), None);
        assert_eq!(GuestMessage::decode(&message), None);
    }
}

#[test]
fn a_peer_and_a_runner_exchange_their_messages_through_the_frames() {
    // A workload that echoes its input to both streams, through frame credit and one shared
    // output pool, with every message encoded as its frame and decoded on the other side.
    let now = Instant::now();
    let limits = Limits::new(5, Window::frames(2), Window::frames(3))
        .unwrap()
        .shared_output();
    let start = Start::new(KEY, limits, timers(), ["/bin/cat"]).with_input();
    let frame = HostMessage::Start(start.clone()).encode();
    let Some(Ok(HostMessage::Start(started))) = HostMessage::decode(&frame) else {
        panic!("{frame:?}");
    };
    assert_eq!(started, start);
    let mut peer = Peer::new(KEY, limits, timers(), now);
    let mut runner = Runner::new(KEY, started.limits, started.timers, now);
    runner.launched(Ok(()));
    let input = (0..200u8).collect::<Vec<_>>();
    let mut sent = 0;
    let mut workload = Vec::new();
    let mut output = [Vec::new(), Vec::new()];
    let mut buffer = [0u8; 64];
    let mut outcome = None;
    for _ in 0..10_000 {
        // The peer's source and messages.
        sent += peer.input(&input[sent..]);
        if sent == input.len() {
            peer.end_input();
        }
        while let Some(message) = peer.next_message(&mut buffer) {
            let frame = HostMessage::from_peer(KEY, message, &buffer)
                .unwrap()
                .encode();
            match HostMessage::decode(&frame).unwrap().unwrap() {
                HostMessage::Credit(key, pool, units) => runner.credit(key, pool, units),
                HostMessage::Input(key, bytes) => runner.input(key, bytes),
                HostMessage::InputEnd(key) => runner.end_input(key),
                HostMessage::Cancel(key, cause) => runner.cancel(now, key, cause),
                HostMessage::Start(start) => panic!("{start:?}"),
            }
            .unwrap();
        }
        // The workload copies its input to both streams, one byte at a time.
        let pending = runner.pending_input().to_vec();
        for byte in pending {
            let stream = Stream::ALL[workload.len() % 2];
            if runner.read_allowance(stream) == 0 {
                break;
            }
            assert_eq!(runner.output(stream, &[byte]), 1);
            workload.push(byte);
            runner.input_written(1);
        }
        if workload.len() == input.len() {
            runner.output_closed(Stream::Stdout);
            runner.output_closed(Stream::Stderr);
            runner.exited(now, Some(Status::Exited(0)));
        }
        while let Some(request) = runner.next_request() {
            if request == runner::Request::Settle {
                runner.settled(Settlement::Settled);
            }
        }
        // The runner's messages, through their frames.
        while let Some(message) = runner.next_message(&mut buffer) {
            let frame = GuestMessage::from_runner(KEY, message, &buffer)
                .unwrap()
                .encode();
            let decoded = GuestMessage::decode(&frame).unwrap().unwrap();
            decoded.deliver(&mut peer, now).unwrap();
        }
        // The peer's sink.
        for (index, stream) in Stream::ALL.into_iter().enumerate() {
            let pending = peer.pending_output(stream).to_vec();
            output[index].extend_from_slice(&pending);
            peer.consumed(stream, pending.len());
        }
        while let Some(event) = peer.next_event() {
            if let peer::Event::Finished(finished) = event {
                outcome = Some(finished);
            }
        }
        if outcome.is_some() {
            break;
        }
    }
    let outcome = outcome.expect("the execution finished");
    assert_eq!(outcome.status(), Some(Status::Exited(0)));
    assert!(outcome.is_reusable());
    let interleaved = output[0]
        .iter()
        .zip(&output[1])
        .flat_map(|(stdout, stderr)| [*stdout, *stderr])
        .collect::<Vec<_>>();
    assert_eq!(interleaved, input);
}

#[test]
fn the_host_s_side_sends_the_cancellation_of_an_execution_that_it_gives_up_on_at_once() {
    // Without grace or settlement time, the host's side gives up on an execution in the step that
    // ends it, for its timeout or its caller's cancellation, and still sends the cancellation.
    let now = Instant::now();
    let immediate = Timers::new(Duration::ZERO, Duration::ZERO);
    let timeout = Duration::from_secs(1);
    for (timers, cause) in [
        (immediate.timeout(timeout), Cause::TimedOut),
        (immediate, Cause::Cancelled),
    ] {
        let start = Start::new(KEY, shared(), timers, ["/bin/program"]);
        let mut execution = HostExecution::new(start, timers, Vec::new(), now);
        // The credit that opens the output's pool.
        assert_eq!(execution.serve(now).len(), 1, "{cause:?}");
        assert!(execution.outcome().is_none(), "{cause:?}");
        let later = now + timeout;
        if cause == Cause::Cancelled {
            execution.peer_mut().cancel(later);
        }
        assert_eq!(
            execution.serve(later),
            [HostMessage::Cancel(KEY, cause).encode()],
            "{cause:?}"
        );
        let outcome = execution.outcome().expect("the host's side gave up");
        assert_eq!((outcome.cause(), outcome.status()), (Some(cause), None));
        assert!(!outcome.is_reusable(), "{cause:?}");
        assert!(execution.serve(later).is_empty(), "{cause:?}");
    }
}

#[test]
fn the_host_s_side_keeps_at_most_max_capture_bytes_of_a_stream_and_cancels_the_rest() {
    // A program that writes a chunk to its standard output at each step, without end: the host's
    // side keeps the first MAX_CAPTURE bytes, then cancels the execution, and its peer, without
    // grace, discards the rest at once. The runner takes and drops what the program writes once
    // it discards the stream, so each step writes once.
    let now = Instant::now();
    let timers = Timers::new(Duration::ZERO, Duration::from_secs(5));
    let start = Start::new(KEY, shared(), timers, ["/usr/bin/yes"]);
    let mut host = HostExecution::new(start, timers, Vec::new(), now);
    let mut runner = Runner::new(KEY, shared(), timers, now);
    runner.launched(Ok(()));
    let written = vec![b'y'; shared().chunk()];
    let mut buffer = vec![0u8; shared().chunk()];
    let mut running = true;
    for _ in 0..2 * MAX_CAPTURE / written.len() {
        if host.outcome().is_some() {
            break;
        }
        for frame in host.serve(now) {
            match HostMessage::decode(&frame).unwrap().unwrap() {
                HostMessage::Credit(key, pool, units) => runner.credit(key, pool, units),
                HostMessage::Cancel(key, cause) => runner.cancel(now, key, cause),
                message => panic!("{message:?}"),
            }
            .unwrap();
        }
        if running {
            runner.output(Stream::Stdout, &written);
        }
        while let Some(request) = runner.next_request() {
            match request {
                runner::Request::Terminate(_) => {
                    running = false;
                    for stream in Stream::ALL {
                        runner.output_closed(stream);
                    }
                    runner.exited(now, Some(Status::Signaled(15)));
                }
                runner::Request::Settle => runner.settled(Settlement::Settled),
                _ => {}
            }
        }
        while let Some(message) = runner.next_message(&mut buffer) {
            let frame = GuestMessage::from_runner(KEY, message, &buffer)
                .unwrap()
                .encode();
            let decoded = GuestMessage::decode(&frame).unwrap().unwrap();
            host.deliver(&decoded, now).unwrap();
        }
    }
    let outcome = host.outcome().expect("the execution finished");
    let kept = host.output(Stream::Stdout);
    assert_eq!(kept.len(), MAX_CAPTURE);
    assert!(kept.iter().all(|byte| *byte == b'y'));
    assert!(host.output(Stream::Stderr).is_empty());
    assert_eq!(outcome.cause(), Some(Cause::Cancelled));
    assert!(outcome.is_truncated(Stream::Stdout));
    assert!(!outcome.is_truncated(Stream::Stderr));
}
