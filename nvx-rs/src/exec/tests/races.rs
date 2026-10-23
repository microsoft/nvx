//! Terminal races: the workload's exit against a cancellation, a timeout, a reset, and a
//! disconnection, in either order, on both sides, each with exactly one terminal outcome, and an
//! exit that arrives once the timeout passed, but before a tick fired it.

use super::*;

/// How a race ends the execution, besides the workload's exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ending {
    /// The peer's cancellation.
    Cancel,
    /// The runner's timeout.
    Timeout,
    /// A reset of the session.
    Reset,
    /// A lost transport.
    Disconnect,
}

impl Ending {
    /// Every ending.
    const ALL: [Self; 4] = [Self::Cancel, Self::Timeout, Self::Reset, Self::Disconnect];

    /// Returns the cause that the ending gives.
    fn cause(self) -> Cause {
        match self {
            Self::Cancel => Cause::Cancelled,
            Self::Timeout => Cause::TimedOut,
            Self::Reset => Cause::Reset,
            Self::Disconnect => Cause::Disconnected,
        }
    }

    /// Applies the ending to `runner` at `now`, which its timeout has reached.
    fn apply(self, runner: &mut Runner, now: Instant) {
        match self {
            Self::Cancel => {
                runner.cancel(now, KEY, Cause::Cancelled).unwrap();
            }
            Self::Timeout => runner.tick(now),
            Self::Reset => runner.reset(now),
            Self::Disconnect => runner.disconnect(now),
        }
    }
}

/// The timeout of the runners in these races.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Drives `runner` from `now` until it finishes, settling it when it asks, granting output credit
/// if `credit`, and moving the clock forward by a grace when it waits, and returns its messages
/// and requests.
fn drive(
    runner: &mut Runner,
    mut now: Instant,
    credit: bool,
) -> (Vec<runner::Message>, Vec<runner::Request>) {
    let mut buffer = [0u8; 64];
    let mut messages = Vec::new();
    let mut requests = Vec::new();
    for _ in 0..16 {
        runner.tick(now);
        while let Some(request) = runner.next_request() {
            requests.push(request);
            if request == runner::Request::Settle {
                runner.settled(Settlement::Settled);
            }
        }
        if credit && !runner.is_detached() && !runner.is_finished() {
            let _ = runner.credit(KEY, Pool::Stdout, 64);
        }
        while let Some(message) = runner.next_message(&mut buffer) {
            messages.push(message);
        }
        if runner.is_finished() {
            break;
        }
        now += GRACE;
    }
    assert!(runner.is_finished(), "the runner finished");
    (messages, requests)
}

#[test]
fn an_exit_racing_each_ending_gives_one_terminal_outcome() {
    for ending in Ending::ALL {
        for exit_first in [true, false] {
            let start = Instant::now();
            // Only the timeout's race gives the runner a timeout, which the exit stops when it
            // comes first.
            let timers = if ending == Ending::Timeout {
                timers().timeout(TIMEOUT)
            } else {
                timers()
            };
            let mut runner = Runner::new(KEY, bytes(64, 64, 64), timers, start);
            runner.launched(Ok(()));
            // Output that waits for credit, which the ending's grace may discard.
            assert_eq!(runner.output(Stream::Stdout, b"abc"), 3);
            runner.output_closed(Stream::Stdout);
            runner.output_closed(Stream::Stderr);
            let now = start + TIMEOUT;
            let status = if exit_first {
                runner.exited(now - Duration::from_millis(1), Some(Status::Exited(3)));
                ending.apply(&mut runner, now);
                Status::Exited(3)
            } else {
                ending.apply(&mut runner, now);
                runner.exited(now, Some(Status::Signaled(9)));
                Status::Signaled(9)
            };
            // A timeout after the exit changes nothing, so the output waits for credit.
            let lossless = exit_first && ending == Ending::Timeout;
            let (messages, requests) = drive(&mut runner, now, lossless);
            let terminals = messages
                .iter()
                .filter_map(|message| match message {
                    runner::Message::Terminal(outcome) => Some(*outcome),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(terminals.len(), 1, "{ending:?} {exit_first}");
            let outcome = terminals[0];
            assert_eq!(outcome.status(), Some(status), "{ending:?} {exit_first}");
            let cause = (!exit_first).then(|| ending.cause());
            assert_eq!(outcome.cause(), cause, "{ending:?} {exit_first}");
            assert_eq!(
                requests
                    .iter()
                    .filter(|request| matches!(request, runner::Request::Terminate(_)))
                    .count(),
                usize::from(!exit_first),
                "{ending:?} {exit_first}"
            );
            assert_eq!(outcome.is_truncated(Stream::Stdout), !lossless);
            assert!(!outcome.is_truncated(Stream::Stderr));
            assert!(outcome.is_reusable());
            // A detached runner sends nothing but keeps its terminal outcome.
            let detached = matches!(ending, Ending::Reset | Ending::Disconnect);
            assert_eq!(messages.len() == 1, detached, "{messages:?}");
            // Nothing follows the terminal outcome.
            assert_eq!(runner.next_message(&mut [0; 64]), None);
            assert_eq!(runner.next_request(), None);
            assert_eq!(runner.deadline(), None);
            assert_eq!(runner.cancel(now, KEY, Cause::Cancelled), Ok(Receipt::Late));
        }
    }
}

#[test]
fn an_exit_that_arrives_once_the_timeout_passed_but_before_a_tick_stops_the_timeout() {
    // The adapter learns of the exit only at the timeout's instant, or a grace after it, and
    // hands it over before it ticks the runner: the end stands as the workload's own, and the
    // output that waits for credit loses nothing.
    for late in [Duration::ZERO, GRACE] {
        let start = Instant::now();
        let mut runner = Runner::new(KEY, bytes(64, 64, 64), timers().timeout(TIMEOUT), start);
        runner.launched(Ok(()));
        assert_eq!(runner.output(Stream::Stdout, b"abc"), 3);
        runner.output_closed(Stream::Stdout);
        runner.output_closed(Stream::Stderr);
        let now = start + TIMEOUT + late;
        runner.exited(now, Some(Status::Exited(0)));
        let (messages, requests) = drive(&mut runner, now, true);
        assert!(
            !requests
                .iter()
                .any(|request| matches!(request, runner::Request::Terminate(_))),
            "{late:?}: {requests:?}"
        );
        let Some(runner::Message::Terminal(outcome)) = messages.last() else {
            panic!("{messages:?}");
        };
        assert_eq!(outcome.status(), Some(Status::Exited(0)));
        assert_eq!(outcome.cause(), None, "{late:?}");
        assert!(!outcome.is_truncated(Stream::Stdout), "{late:?}");
        assert!(outcome.is_reusable());
    }
}

#[test]
fn the_first_cause_wins() {
    let now = Instant::now();
    let mut runner = Runner::new(KEY, bytes(64, 64, 64), timers().timeout(TIMEOUT), now);
    runner.launched(Ok(()));
    runner.tick(now + TIMEOUT);
    runner.cancel(now + TIMEOUT, KEY, Cause::Cancelled).unwrap();
    runner.disconnect(now + TIMEOUT);
    assert_eq!(
        runner.next_request(),
        Some(runner::Request::Terminate(Cause::TimedOut))
    );
    runner.exited(now + TIMEOUT, Some(Status::Signaled(9)));
    runner.output_closed(Stream::Stdout);
    runner.output_closed(Stream::Stderr);
    let (messages, _) = drive(&mut runner, now + TIMEOUT, false);
    let Some(runner::Message::Terminal(outcome)) = messages.last() else {
        panic!("{messages:?}");
    };
    assert_eq!(outcome.cause(), Some(Cause::TimedOut));
}

#[test]
fn the_peer_finishes_once_whether_its_cancellation_or_the_terminal_outcome_comes_first() {
    let terminal = |cause: Option<Cause>| {
        let outcome = Outcome::new(Some(Status::Signaled(9))).with_reusable();
        match cause {
            Some(cause) => outcome.with_cause(cause),
            None => outcome,
        }
    };
    for cancel_first in [true, false] {
        for runner_cause in [None, Some(Cause::Cancelled), Some(Cause::TimedOut)] {
            let now = Instant::now();
            let mut peer = Peer::new(KEY, bytes(8, 8, 8), timers(), now);
            while peer.next_message(&mut []).is_some() {}
            peer.output(KEY, Stream::Stdout, b"held").unwrap();
            peer.end_output(KEY, Stream::Stdout).unwrap();
            peer.end_output(KEY, Stream::Stderr).unwrap();
            if cancel_first {
                peer.cancel(now);
                assert_eq!(
                    peer.next_message(&mut []),
                    Some(peer::Message::Cancel(Cause::Cancelled))
                );
                peer.terminal(now, KEY, terminal(runner_cause)).unwrap();
            } else {
                peer.terminal(now, KEY, terminal(runner_cause)).unwrap();
                peer.cancel(now);
                // The runner finished, so the peer sends no cancellation.
                assert_eq!(peer.next_message(&mut []), None);
            }
            // The stalled sink holds the output for the grace, then the peer discards it.
            assert!(
                !std::iter::from_fn(|| peer.next_event())
                    .any(|event| matches!(event, peer::Event::Finished(_)))
            );
            peer.tick(now + GRACE);
            let finished = std::iter::from_fn(|| peer.next_event())
                .filter_map(|event| match event {
                    peer::Event::Finished(outcome) => Some(outcome),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(finished.len(), 1);
            let outcome = finished[0];
            // The runner's cause stands; the peer's own fills in for none.
            assert_eq!(outcome.cause(), runner_cause.or(Some(Cause::Cancelled)));
            assert!(outcome.is_truncated(Stream::Stdout));
            assert!(!outcome.is_truncated(Stream::Stderr));
            assert!(outcome.is_reusable());
            assert_eq!(peer.next_event(), None);
        }
    }
}

#[test]
fn a_peer_detached_before_the_terminal_outcome_finishes_on_its_own() {
    for ending in [Ending::Reset, Ending::Disconnect] {
        let now = Instant::now();
        let mut peer = Peer::new(KEY, bytes(8, 8, 8), timers(), now);
        while peer.next_message(&mut []).is_some() {}
        peer.output(KEY, Stream::Stdout, b"held").unwrap();
        peer.end_output(KEY, Stream::Stderr).unwrap();
        match ending {
            Ending::Reset => peer.reset(now),
            _ => peer.disconnect(now),
        }
        // The sink still takes what arrived, for the grace.
        assert_eq!(peer.pending_output(Stream::Stdout), b"held");
        peer.consumed(Stream::Stdout, 4);
        let events = std::iter::from_fn(|| peer.next_event()).collect::<Vec<_>>();
        let Some(peer::Event::Finished(outcome)) = events.last() else {
            panic!("{events:?}");
        };
        // Standard output never ended, so it may have lost bytes; standard error ended whole.
        assert_eq!(outcome.status(), None);
        assert_eq!(outcome.cause(), Some(ending.cause()));
        assert!(outcome.is_truncated(Stream::Stdout));
        assert!(outcome.is_truncated(Stream::Stderr));
        assert!(!outcome.is_reusable());
        assert_eq!(peer.next_message(&mut [0; 8]), None);
        // What arrives after the detachment comes too late.
        assert_eq!(
            peer.terminal(now, KEY, Outcome::new(None)),
            Ok(Receipt::Late)
        );
    }
}

#[test]
fn the_runner_s_messages_after_the_peer_detached_are_late_while_its_sink_drains() {
    for ending in [Ending::Reset, Ending::Disconnect] {
        let now = Instant::now();
        let mut peer = Peer::new(KEY, bytes(8, 8, 8), timers(), now);
        while peer.next_message(&mut []).is_some() {}
        peer.output(KEY, Stream::Stdout, b"held").unwrap();
        match ending {
            Ending::Reset => peer.reset(now),
            _ => peer.disconnect(now),
        }
        // The runner's last messages arrive while the sink still holds what came before, and
        // before the caller retrieved the peer's outcome: the peer's own outcome stands.
        assert_eq!(peer.output(KEY, Stream::Stdout, b"more"), Ok(Receipt::Late));
        for stream in Stream::ALL {
            assert_eq!(peer.end_output(KEY, stream), Ok(Receipt::Late));
        }
        let terminal = Outcome::new(Some(Status::Exited(0))).with_reusable();
        assert_eq!(peer.terminal(now, KEY, terminal), Ok(Receipt::Late));
        assert_eq!(peer.pending_output(Stream::Stdout), b"held");
        peer.consumed(Stream::Stdout, 4);
        let events = std::iter::from_fn(|| peer.next_event()).collect::<Vec<_>>();
        let Some(peer::Event::Finished(outcome)) = events.last() else {
            panic!("{events:?}");
        };
        assert_eq!(outcome.status(), None, "{ending:?}");
        assert_eq!(outcome.cause(), Some(ending.cause()));
        assert!(outcome.is_truncated(Stream::Stdout) && outcome.is_truncated(Stream::Stderr));
        assert!(!outcome.is_reusable());
    }
}

#[test]
fn the_runner_s_messages_after_the_peer_gave_up_on_it_are_late() {
    // The runner answers the cancellation only once the peer gave up waiting for it, but before
    // the peer's caller retrieved its outcome: the peer's own outcome stands.
    let now = Instant::now();
    let mut peer = Peer::new(KEY, bytes(8, 8, 8), timers(), now);
    while peer.next_message(&mut []).is_some() {}
    peer.started(KEY).unwrap();
    peer.cancel(now);
    assert_eq!(
        peer.next_message(&mut []),
        Some(peer::Message::Cancel(Cause::Cancelled))
    );
    let late = now + GRACE + SETTLEMENT;
    peer.tick(late);
    assert_eq!(peer.output(KEY, Stream::Stdout, b"late"), Ok(Receipt::Late));
    for stream in Stream::ALL {
        assert_eq!(peer.end_output(KEY, stream), Ok(Receipt::Late));
    }
    let terminal = Outcome::new(Some(Status::Exited(0))).with_reusable();
    assert_eq!(peer.terminal(late, KEY, terminal), Ok(Receipt::Late));
    let events = std::iter::from_fn(|| peer.next_event()).collect::<Vec<_>>();
    let Some(peer::Event::Finished(outcome)) = events.last() else {
        panic!("{events:?}");
    };
    assert_eq!(outcome.status(), None);
    assert_eq!(outcome.cause(), Some(Cause::Cancelled));
    assert!(outcome.is_truncated(Stream::Stdout) && outcome.is_truncated(Stream::Stderr));
    assert!(!outcome.is_reusable());
}
