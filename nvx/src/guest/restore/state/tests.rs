//! Tests of a clone's fence-free guest restore, with records that each test builds with
//! `nvx::guest_restore`, a fake clock, whose instants each test takes after a base instant, and a
//! fake adapter of a framing of the tests' own, whose transport delivers the host's bytes when and
//! as a test chooses, and whose output takes nothing.

use super::*;
use crate::guest_restore::{
    Configuration, Image, MAX_RECORD_BYTES, RestoreSection, TransactionId, Upper,
};
use crate::protocol::root::OverlayMode;
use crate::protocol::view::{MachineIdMode, View};

/// The number of online CPUs of the clone.
const CPUS: u32 = 2;
/// The memory size of the clone: eight online memory blocks of 128 MiB.
const MEMORY: u64 = 8 << 27;
/// How long the activation may take after the restore's acknowledgement.
const TIMEOUT: Duration = Duration::from_secs(10);
/// The transaction of the clone's restore.
const TRANSACTION: TransactionId = TransactionId::from_bytes([0x22; TransactionId::LEN]);
/// The transaction of another restore.
const OTHER_TRANSACTION: TransactionId = TransactionId::from_bytes([0x33; TransactionId::LEN]);
/// The kind of the host's request for Ready in the tests' framing.
const READY: u8 = 1;
/// The kind of any other request of the host in the tests' framing.
const PING: u8 = 2;

/// Returns the activation in a state that it reached at an instant.
type Setup = fn(Instant) -> GuestRestore;

/// Returns whether a failure's cause is the one that a test expects.
type Expected = fn(&GuestRestoreCause) -> bool;

/// Returns the instant `seconds` seconds after `base`.
fn after(base: Instant, seconds: u64) -> Instant {
    base + Duration::from_secs(seconds)
}

/// Returns the resources that the clone has, as its repair reports them.
fn resources() -> ExpectedResources {
    ExpectedResources {
        cpus: CPUS,
        memory: MEMORY,
    }
}

/// Returns the configuration of a root over a tmpfs upper, named `hostname`, that expects
/// `resources`.
fn configuration(hostname: &[u8], resources: ExpectedResources) -> Configuration {
    Configuration {
        image: Image::Erofs { uuid: None },
        upper: Upper::Tmpfs {
            max_memory_percent: 50,
        },
        overlay: OverlayMode::UntrustedLowers,
        export: None,
        mappings: 0,
        network: None,
        view: View {
            hostname: hostname.to_vec(),
            machine_id: MachineIdMode::Image,
        },
        resources,
    }
}

/// Returns the record of a clone's restore of `transaction`, of a root named `hostname` that
/// expects `resources`.
fn clone_record(
    transaction: TransactionId,
    hostname: &[u8],
    resources: ExpectedResources,
) -> GuestRestoreRecord {
    let restore = RestoreSection { transaction };
    GuestRestoreRecord::new(Some(restore), configuration(hostname, resources)).unwrap()
}

/// Returns the record of the clone's restore packet: the clone's restore of [`TRANSACTION`],
/// which expects the clone's resources.
fn accepted() -> GuestRestoreRecord {
    clone_record(TRANSACTION, b"clone", resources())
}

/// Returns the bytes of the record of the clone's restore packet.
fn record() -> Vec<u8> {
    accepted().encode()
}

/// Returns what Ready reports about the activation of the packet's record.
fn reported() -> GuestRestoreReady {
    GuestRestoreReady::of(&accepted())
}

/// Returns the activation, which awaits the packet's record, of the clone whose repair
/// acknowledged its restore at `acknowledged`.
fn pending(acknowledged: Instant) -> GuestRestore {
    GuestRestore::with_resources(resources(), Some(acknowledged), TIMEOUT, acknowledged)
}

/// Returns the activation of [`pending`] once it accepted the packet's record at `at`.
fn activating(at: Instant) -> GuestRestore {
    let mut activation = pending(at);
    let record = activation.accept(at, Some(&record())).unwrap();
    assert_eq!(*record, accepted());
    assert_eq!(activation.state(), GuestRestoreState::Activating);
    activation
}

/// Returns the activation of [`activating`] once its application of the record completed at
/// `at`, before the host asked for Ready.
fn applied(at: Instant) -> GuestRestore {
    let mut activation = activating(at);
    assert_eq!(activation.complete(at, Ok(())).unwrap(), reported());
    assert_eq!(activation.state(), GuestRestoreState::Applied);
    activation
}

/// Returns the activation of [`applied`] once the host asked for Ready at `at`.
fn ready(at: Instant) -> GuestRestore {
    let mut activation = applied(at);
    let action = activation.input(at, GuestRestoreInput::Ready).unwrap();
    assert_eq!(action, GuestRestoreAction::Ready(reported()));
    assert_eq!(activation.state(), GuestRestoreState::Ready);
    activation
}

/// Returns the activation of [`ready`] once the adapter's order started the sandbox.
fn started(at: Instant) -> GuestRestore {
    let mut activation = ready(at);
    activation.start().unwrap();
    assert_eq!(activation.state(), GuestRestoreState::Started);
    activation
}

/// Checks that `activation` failed finally with `failure` at `now`: it has no deadline or
/// established activation, and every later call returns the same failure.
fn assert_final(activation: &mut GuestRestore, failure: &GuestRestoreFailure, now: Instant) {
    assert_ne!(failure.state(), GuestRestoreState::Failed);
    assert_eq!(activation.state(), GuestRestoreState::Failed);
    assert_eq!(activation.deadline(), None);
    assert_eq!(activation.established(), None);
    let record = record();
    let later = [
        activation.input(now, GuestRestoreInput::Flow).unwrap_err(),
        activation.input(now, GuestRestoreInput::Reset).unwrap_err(),
        activation.input(now, GuestRestoreInput::Ready).unwrap_err(),
        activation
            .input(now, GuestRestoreInput::Request)
            .unwrap_err(),
        activation.accept(now, Some(&record)).map(drop).unwrap_err(),
        activation.complete(now, Ok(())).unwrap_err(),
        activation.start().unwrap_err(),
        activation.tick(now).unwrap_err(),
    ];
    for later in later {
        assert_eq!(later.state(), failure.state());
        assert!(Arc::ptr_eq(&later.cause, &failure.cause));
    }
}

#[test]
fn a_clone_activates_from_its_packet_before_the_host_asks_and_becomes_ready_without_a_fence() {
    let t0 = Instant::now();

    let mut activation = pending(t0);
    assert_eq!(activation.state(), GuestRestoreState::Pending);
    assert_eq!(activation.deadline(), Some(after(t0, 10)));
    // The packet's record, which the activation decoded, goes back to the adapter, which applies
    // it, before anything of the session.
    let record = activation.accept(after(t0, 1), Some(&record())).unwrap();
    assert_eq!(*record, accepted());
    assert_eq!(activation.state(), GuestRestoreState::Activating);
    assert_eq!(activation.deadline(), Some(after(t0, 10)));
    assert_eq!(activation.established(), None);
    // Once the application succeeded, the activation awaits the host's request for Ready, by the
    // same deadline.
    let ready = activation.complete(after(t0, 2), Ok(())).unwrap();
    assert_eq!(ready, reported());
    assert_eq!(ready.transaction, Some(TRANSACTION));
    assert_eq!(activation.state(), GuestRestoreState::Applied);
    assert_eq!(activation.deadline(), Some(after(t0, 10)));
    assert_eq!(activation.established(), None);
    // The session's reset, which the restore queued, and its flow, change nothing of the record.
    let reset = activation.input(after(t0, 3), GuestRestoreInput::Reset);
    assert_eq!(reset.unwrap(), GuestRestoreAction::Acknowledge);
    let flow = activation.input(after(t0, 3), GuestRestoreInput::Flow);
    assert_eq!(flow.unwrap(), GuestRestoreAction::Continue);
    assert_eq!(activation.state(), GuestRestoreState::Applied);
    // The host's request for Ready makes the clone ready, and gets what Ready reports.
    let ready = activation.input(after(t0, 4), GuestRestoreInput::Ready);
    assert_eq!(ready.unwrap(), GuestRestoreAction::Ready(reported()));
    assert_eq!(activation.state(), GuestRestoreState::Ready);
    assert_eq!(activation.established(), Some(reported()));
    // No deadline holds a ready clone: the host's mapping table and its start go to the adapter's
    // order, whenever they come.
    assert_eq!(activation.deadline(), None);
    activation.tick(after(t0, 3600)).unwrap();
    let request = activation.input(after(t0, 3600), GuestRestoreInput::Request);
    assert_eq!(request.unwrap(), GuestRestoreAction::Serve);
    activation.start().unwrap();
    assert_eq!(activation.state(), GuestRestoreState::Started);
    assert_eq!(activation.established(), Some(reported()));
    let request = activation.input(after(t0, 7200), GuestRestoreInput::Request);
    assert_eq!(request.unwrap(), GuestRestoreAction::Serve);
    let flow = activation.input(after(t0, 7200), GuestRestoreInput::Flow);
    assert_eq!(flow.unwrap(), GuestRestoreAction::Continue);
}

#[test]
fn a_request_for_ready_before_the_record_applied_joins_the_activation_of_its_session() {
    let t0 = Instant::now();
    // A request for Ready while the record awaits acceptance, or applies, joins the activation,
    // whose completion makes the clone ready, and answers it.
    let states: [Setup; 2] = [pending, activating];
    for activation in states {
        let mut activation = activation(t0);
        let join = activation.input(after(t0, 1), GuestRestoreInput::Ready);
        assert_eq!(join.unwrap(), GuestRestoreAction::Join);
        if activation.state() == GuestRestoreState::Pending {
            activation.accept(after(t0, 2), Some(&record())).unwrap();
        }
        assert_eq!(
            activation.complete(after(t0, 3), Ok(())).unwrap(),
            reported()
        );
        assert_eq!(activation.state(), GuestRestoreState::Ready);
        assert_eq!(activation.established(), Some(reported()));
        assert_eq!(activation.deadline(), None);
    }
    // A reset abandons the request of the earlier session, which the host asks again in the
    // later one: the activation completes applied, and awaits it.
    let mut activation = activating(t0);
    let join = activation.input(after(t0, 1), GuestRestoreInput::Ready);
    assert_eq!(join.unwrap(), GuestRestoreAction::Join);
    let reset = activation.input(after(t0, 2), GuestRestoreInput::Reset);
    assert_eq!(reset.unwrap(), GuestRestoreAction::Acknowledge);
    activation.complete(after(t0, 3), Ok(())).unwrap();
    assert_eq!(activation.state(), GuestRestoreState::Applied);
    let ready = activation.input(after(t0, 4), GuestRestoreInput::Ready);
    assert_eq!(ready.unwrap(), GuestRestoreAction::Ready(reported()));
}

#[test]
fn no_other_request_is_accepted_before_ready() {
    let t0 = Instant::now();
    // Before the clone is ready, nothing but the host's request for Ready.
    let states: [Setup; 3] = [pending, activating, applied];
    for activation in states {
        let mut activation = activation(t0);
        let state = activation.state();
        let failure = activation
            .input(after(t0, 1), GuestRestoreInput::Request)
            .unwrap_err();
        assert_eq!(failure.state(), state);
        assert!(matches!(failure.cause(), GuestRestoreCause::Request));
        assert_final(&mut activation, &failure, after(t0, 2));
    }
}

#[test]
fn resets_start_later_sessions_whose_requests_get_the_same_ready() {
    let t0 = Instant::now();
    // A reset at any point before the clone is ready, such as once the host's attachment ended,
    // changes nothing of the record, which came with the restore.
    let states: [Setup; 3] = [pending, activating, applied];
    for activation in states {
        let mut activation = activation(t0);
        let state = activation.state();
        for second in 1..=3 {
            let reset = activation.input(after(t0, second), GuestRestoreInput::Reset);
            assert_eq!(reset.unwrap(), GuestRestoreAction::Acknowledge);
            assert_eq!(activation.state(), state);
            assert_eq!(activation.deadline(), Some(after(t0, 10)));
        }
    }
    // Once the clone is ready, or started, each later session gets the established activation and
    // transaction without activating again.
    let states: [Setup; 2] = [ready, started];
    for activation in states {
        let mut activation = activation(t0);
        let state = activation.state();
        for session in 1..=3 {
            let now = after(t0, 60 * session);
            let reset = activation.input(now, GuestRestoreInput::Reset);
            assert_eq!(reset.unwrap(), GuestRestoreAction::Acknowledge);
            assert_eq!(activation.state(), state);
            assert_eq!(activation.established(), Some(reported()));
            let again = activation.input(now, GuestRestoreInput::Ready);
            assert_eq!(again.unwrap(), GuestRestoreAction::Ready(reported()));
            let request = activation.input(now, GuestRestoreInput::Request);
            assert_eq!(request.unwrap(), GuestRestoreAction::Serve);
        }
    }
}

#[test]
fn a_failed_application_is_final_and_nothing_applies_the_record_again() {
    let t0 = Instant::now();

    let mut activation = activating(t0);
    let failure = activation
        .complete(after(t0, 1), Err("the root's assembly failed".into()))
        .unwrap_err();
    assert_eq!(failure.state(), GuestRestoreState::Activating);
    let GuestRestoreCause::Failed(error) = failure.cause() else {
        panic!("unexpected cause: {failure}");
    };
    assert_eq!(error.to_string(), "the root's assembly failed");
    assert_eq!(
        failure.source().unwrap().to_string(),
        "the root's assembly failed"
    );
    // Neither a request for Ready nor the packet's record applies the record again: each gets the
    // failure.
    assert_final(&mut activation, &failure, after(t0, 2));
}

#[test]
fn the_packet_s_record_is_decoded_and_validated_before_any_side_effect() {
    let t0 = Instant::now();
    let mut long = record();
    long.resize(MAX_RECORD_BYTES + 1, 0);
    let mut version = record();
    version[0] = 1;
    let cold = GuestRestoreRecord::new(None, configuration(b"clone", resources()))
        .unwrap()
        .encode();
    let more_cpus = ExpectedResources {
        cpus: CPUS + 2,
        memory: MEMORY,
    };
    let less_memory = ExpectedResources {
        cpus: CPUS,
        memory: MEMORY - (128 << 20),
    };
    // The clone's memory counts whole blocks of 128 MiB, so it never has a part of one.
    let partial_block = ExpectedResources {
        cpus: CPUS,
        memory: MEMORY + (64 << 20),
    };
    let cases: [(Option<Vec<u8>>, Expected); 9] = [
        (None, |cause| matches!(cause, GuestRestoreCause::Missing)),
        (Some(Vec::new()), |cause| {
            matches!(
                cause,
                GuestRestoreCause::Record(GuestRestoreError::Truncated)
            )
        }),
        (Some(long), |cause| {
            matches!(
                cause,
                GuestRestoreCause::Record(GuestRestoreError::TooLong(1025))
            )
        }),
        (Some(version), |cause| {
            matches!(
                cause,
                GuestRestoreCause::Record(GuestRestoreError::Version(1))
            )
        }),
        (Some(cold), |cause| matches!(cause, GuestRestoreCause::Cold)),
        (
            Some(clone_record(TRANSACTION, b"clone", more_cpus).encode()),
            |cause| {
                matches!(
                    cause,
                    GuestRestoreCause::Resources {
                        expected: ExpectedResources { cpus: 4, .. },
                        actual: ExpectedResources {
                            cpus: CPUS,
                            memory: MEMORY
                        },
                    }
                )
            },
        ),
        (
            Some(clone_record(TRANSACTION, b"clone", less_memory).encode()),
            |cause| matches!(cause, GuestRestoreCause::Resources { expected, .. } if expected.memory == MEMORY - (128 << 20)),
        ),
        (
            Some(clone_record(TRANSACTION, b"clone", partial_block).encode()),
            |cause| matches!(cause, GuestRestoreCause::Resources { expected, .. } if expected.memory == MEMORY + (64 << 20)),
        ),
        (
            Some(clone_record(OTHER_TRANSACTION, b"clone", more_cpus).encode()),
            |cause| matches!(cause, GuestRestoreCause::Resources { .. }),
        ),
    ];
    for (bytes, cause) in cases {
        let mut activation = pending(t0);
        let failure = activation
            .accept(after(t0, 1), bytes.as_deref())
            .unwrap_err();
        assert_eq!(failure.state(), GuestRestoreState::Pending);
        assert!(cause(failure.cause()), "{failure}");
        assert_final(&mut activation, &failure, after(t0, 2));
    }
    // A valid record goes to the adapter exactly as it decodes, and opens nothing yet.
    let mut activation = pending(t0);
    let accepted = activation.accept(after(t0, 1), Some(&record())).unwrap();
    assert_eq!(*accepted, GuestRestoreRecord::decode(&record()).unwrap());
    assert_eq!(activation.established(), None);
}

#[test]
fn the_deadline_starts_at_the_acknowledgement_and_holds_until_the_host_asks() {
    let t0 = Instant::now();

    // The repair acknowledged the restore at `t0`, and the adapter made the activation later.
    let mut activation = GuestRestore::with_resources(resources(), Some(t0), TIMEOUT, after(t0, 3));
    assert_eq!(activation.deadline(), Some(after(t0, 10)));
    activation.accept(after(t0, 8), Some(&record())).unwrap();
    activation.complete(after(t0, 9), Ok(())).unwrap();
    activation
        .input(after(t0, 9), GuestRestoreInput::Reset)
        .unwrap();
    let late = after(t0, 9) + Duration::from_millis(999);
    activation.tick(late).unwrap();
    assert_eq!(activation.deadline(), Some(after(t0, 10)));
    // At the deadline, the applied activation of a clone whose host never asked fails.
    let failure = activation.tick(after(t0, 10)).unwrap_err();
    assert_eq!(failure.state(), GuestRestoreState::Applied);
    assert!(matches!(failure.cause(), GuestRestoreCause::Expired));
    assert_final(&mut activation, &failure, after(t0, 11));
    // An application that completes at the deadline completes too late, as a request for Ready
    // does, and the packet's record accepted at the deadline is too late too.
    let late: [(Setup, Failing); 3] = [
        (
            activating,
            Box::new(move |activation| activation.complete(after(t0, 10), Ok(())).map(drop)),
        ),
        (
            applied,
            Box::new(move |activation| {
                activation
                    .input(after(t0, 10), GuestRestoreInput::Ready)
                    .map(drop)
            }),
        ),
        (
            pending,
            Box::new(move |activation| activation.accept(after(t0, 10), Some(&record())).map(drop)),
        ),
    ];
    for (activation, fail) in late {
        let mut activation = activation(t0);
        let state = activation.state();
        let failure = fail(&mut activation).unwrap_err();
        assert_eq!(failure.state(), state);
        assert!(matches!(failure.cause(), GuestRestoreCause::Expired));
        assert_final(&mut activation, &failure, after(t0, 11));
    }
}

#[test]
fn an_ungated_restore_s_deadline_starts_at_the_activation() {
    let t0 = Instant::now();

    // Without an acknowledgement, the deadline starts when the adapter makes the activation.
    let mut activation = GuestRestore::with_resources(resources(), None, TIMEOUT, after(t0, 5));
    assert_eq!(activation.deadline(), Some(after(t0, 15)));
    activation.tick(after(t0, 14)).unwrap();
    let failure = activation
        .input(after(t0, 15), GuestRestoreInput::Reset)
        .unwrap_err();
    assert_eq!(failure.state(), GuestRestoreState::Pending);
    assert!(matches!(failure.cause(), GuestRestoreCause::Expired));
    assert_final(&mut activation, &failure, after(t0, 16));
    // A deadline that no instant can hold never passes.
    let mut activation = GuestRestore::with_resources(resources(), Some(t0), Duration::MAX, t0);
    assert_eq!(activation.deadline(), None);
    activation.tick(after(t0, 1 << 30)).unwrap();
    let reset = activation.input(after(t0, 1 << 30), GuestRestoreInput::Reset);
    assert_eq!(reset.unwrap(), GuestRestoreAction::Acknowledge);
}

#[test]
fn out_of_order_acceptances_completions_and_starts_are_final() {
    let t0 = Instant::now();
    // An activation takes one record, completes only while it applies it, and the sandbox starts
    // only once it is ready, and once.
    let acceptances: [Setup; 4] = [activating, applied, ready, started];
    for activation in acceptances {
        let mut activation = activation(t0);
        let state = activation.state();
        let failure = activation
            .accept(after(t0, 1), Some(&record()))
            .unwrap_err();
        assert_eq!(failure.state(), state);
        assert!(matches!(failure.cause(), GuestRestoreCause::Accept));
        assert_final(&mut activation, &failure, after(t0, 2));
    }
    let completions: [Setup; 4] = [pending, applied, ready, started];
    for activation in completions {
        let mut activation = activation(t0);
        let state = activation.state();
        let failure = activation.complete(after(t0, 1), Ok(())).unwrap_err();
        assert_eq!(failure.state(), state);
        assert!(matches!(failure.cause(), GuestRestoreCause::Completion));
        assert_final(&mut activation, &failure, after(t0, 2));
    }
    let starts: [Setup; 4] = [pending, activating, applied, started];
    for activation in starts {
        let mut activation = activation(t0);
        let state = activation.state();
        let failure = activation.start().unwrap_err();
        assert_eq!(failure.state(), state);
        assert!(matches!(failure.cause(), GuestRestoreCause::Start));
        assert_final(&mut activation, &failure, after(t0, 2));
    }
}

/// Returns `body` as a message of the tests' own framing, which no adapter of NVX's speaks: its
/// `kind`, its length, a little-endian `u16`, and the body.
fn message(kind: u8, body: &[u8]) -> Vec<u8> {
    let length = u16::try_from(body.len()).unwrap();
    let mut message = vec![kind];
    message.extend_from_slice(&length.to_le_bytes());
    message.extend_from_slice(body);
    message
}

/// A reply of the tests' adapter to the host.
#[derive(Debug, PartialEq, Eq)]
enum Reply {
    /// The acknowledgement of the session's reset.
    Acknowledgement,
    /// Ready, which reports the activation.
    Ready(GuestRestoreReady),
}

/// An adapter of the tests' framing, which drives an activation as an adapter's loop does: it
/// takes the host's bytes as its transport delivers them, passes the activation each message
/// that they complete, and flow control for each byte that completes none, counts the requests
/// for Ready of the current session that joined the activation, and queues its replies for an
/// output that takes nothing, as a host that reads nothing leaves it, and which it never waits
/// for.
struct Adapter {
    /// The activation.
    activation: GuestRestore,
    /// The bytes of the host's message in progress.
    message: Vec<u8>,
    /// The replies that wait for the output.
    output: Vec<Reply>,
    /// The requests for Ready of the current session that joined the activation.
    joined: usize,
}

impl Adapter {
    /// Returns the adapter that drives `activation`.
    fn new(activation: GuestRestore) -> Self {
        Self {
            activation,
            message: Vec::new(),
            output: Vec::new(),
            joined: 0,
        }
    }

    /// Takes the session's reset at `now`.
    fn reset(&mut self, now: Instant) -> Result<(), GuestRestoreFailure> {
        let action = self.activation.input(now, GuestRestoreInput::Reset)?;
        self.act(action);
        Ok(())
    }

    /// Takes `bytes` of the host's messages, which the transport delivered at `now`, one at a
    /// time.
    fn receive(&mut self, now: Instant, bytes: &[u8]) -> Result<(), GuestRestoreFailure> {
        for &byte in bytes {
            self.message.push(byte);
            let complete = match self.message.as_slice() {
                [_, low, high, body @ ..] => {
                    body.len() == usize::from(u16::from_le_bytes([*low, *high]))
                }
                _ => false,
            };
            let action = if complete {
                let message = std::mem::take(&mut self.message);
                let input = match message.as_slice() {
                    [READY, ..] => GuestRestoreInput::Ready,
                    _ => GuestRestoreInput::Request,
                };
                self.activation.input(now, input)?
            } else {
                self.activation.input(now, GuestRestoreInput::Flow)?
            };
            self.act(action);
        }
        Ok(())
    }

    /// Completes the application of the accepted record at `now`, and answers each request for
    /// Ready that joined it.
    fn complete(&mut self, now: Instant) -> Result<(), GuestRestoreFailure> {
        let ready = self.activation.complete(now, Ok(()))?;
        for _ in 0..std::mem::take(&mut self.joined) {
            self.output.push(Reply::Ready(ready));
        }
        Ok(())
    }

    /// Does what `action` says.
    fn act(&mut self, action: GuestRestoreAction) {
        match action {
            GuestRestoreAction::Acknowledge => {
                self.joined = 0;
                self.output.push(Reply::Acknowledgement);
            }
            GuestRestoreAction::Join => self.joined += 1,
            GuestRestoreAction::Continue | GuestRestoreAction::Serve => {}
            GuestRestoreAction::Ready(ready) => self.output.push(Reply::Ready(ready)),
        }
    }
}

#[test]
fn the_deadline_stays_live_under_trickled_input_and_blocked_output() {
    let t0 = Instant::now();
    let ready = message(READY, b"");
    let ping = message(PING, b"");
    // A host that trickles its request for Ready, a byte a second, never completes it by the
    // deadline: the byte at the deadline fails the applied activation, while the output took
    // nothing but the acknowledgement.
    let mut adapter = Adapter::new(applied(t0));
    adapter.reset(t0).unwrap();
    let mut failure = None;
    for second in 1.. {
        if let Err(error) = adapter.receive(after(t0, second), &ready[..1]) {
            failure = Some((second, error));
            break;
        }
    }
    let (second, failure) = failure.unwrap();
    assert_eq!(second, 10);
    assert_eq!(failure.state(), GuestRestoreState::Applied);
    assert!(matches!(failure.cause(), GuestRestoreCause::Expired));
    assert_eq!(adapter.output, [Reply::Acknowledgement]);
    assert_final(&mut adapter.activation, &failure, after(t0, 11));
    // A host that sends nothing leaves the adapter only its wake at the deadline, which fails the
    // activation as well.
    let mut adapter = Adapter::new(applied(t0));
    adapter.reset(after(t0, 1)).unwrap();
    let deadline = adapter.activation.deadline().unwrap();
    assert_eq!(deadline, after(t0, 10));
    adapter
        .activation
        .tick(deadline - Duration::from_millis(1))
        .unwrap();
    let failure = adapter.activation.tick(deadline).unwrap_err();
    assert_eq!(failure.state(), GuestRestoreState::Applied);
    assert!(matches!(failure.cause(), GuestRestoreCause::Expired));
    assert_final(&mut adapter.activation, &failure, after(t0, 11));
    // A host whose request for Ready arrives while the record applies gets one Ready once the
    // application completes, and a later session's request gets the same Ready.
    let mut adapter = Adapter::new(activating(t0));
    adapter.reset(t0).unwrap();
    adapter.receive(after(t0, 1), &ready).unwrap();
    assert_eq!(adapter.joined, 1);
    adapter.complete(after(t0, 2)).unwrap();
    adapter.reset(after(t0, 3)).unwrap();
    adapter.receive(after(t0, 4), &ready).unwrap();
    assert_eq!(
        adapter.output,
        [
            Reply::Acknowledgement,
            Reply::Ready(reported()),
            Reply::Acknowledgement,
            Reply::Ready(reported()),
        ]
    );
    assert_eq!(adapter.activation.state(), GuestRestoreState::Ready);
    // Another message of the framing, whole, before the clone is ready fails the activation.
    let mut adapter = Adapter::new(applied(t0));
    let failure = adapter.receive(after(t0, 1), &ping).unwrap_err();
    assert_eq!(failure.state(), GuestRestoreState::Applied);
    assert!(matches!(failure.cause(), GuestRestoreCause::Request));
    assert_final(&mut adapter.activation, &failure, after(t0, 2));
}

/// A call that fails an activation.
type Failing = Box<dyn Fn(&mut GuestRestore) -> Result<(), GuestRestoreFailure>>;

#[test]
fn strict_failures_are_returned_and_never_power_off() {
    let t0 = Instant::now();
    let cold = GuestRestoreRecord::new(None, configuration(b"clone", resources()))
        .unwrap()
        .encode();
    let more_cpus = ExpectedResources {
        cpus: CPUS + 1,
        memory: MEMORY,
    };
    let unexpected = clone_record(TRANSACTION, b"clone", more_cpus).encode();
    let accept = move |record: Option<Vec<u8>>| -> Failing {
        Box::new(move |activation| activation.accept(after(t0, 1), record.as_deref()).map(drop))
    };
    // Each failure, in a state in which it comes: the activation returns it, keeps it, and changes
    // nothing else. It has no system through which it could power the VM off: the adapter reports
    // the failure, and ends the VM.
    let failures: [(Setup, Failing); 10] = [
        (
            applied,
            Box::new(move |activation| activation.tick(after(t0, 10))),
        ),
        (
            applied,
            Box::new(move |activation| {
                activation
                    .input(after(t0, 1), GuestRestoreInput::Request)
                    .map(drop)
            }),
        ),
        (pending, accept(None)),
        (pending, accept(Some(Vec::new()))),
        (pending, accept(Some(cold))),
        (pending, accept(Some(unexpected))),
        (
            activating,
            Box::new(move |activation| {
                activation
                    .complete(after(t0, 1), Err("failed".into()))
                    .map(drop)
            }),
        ),
        (ready, accept(Some(record()))),
        (
            pending,
            Box::new(move |activation| activation.complete(after(t0, 1), Ok(())).map(drop)),
        ),
        (activating, Box::new(|activation| activation.start())),
    ];
    let mut causes = Vec::new();
    for (activation, fail) in failures {
        let mut activation = activation(t0);

        let failure = fail(&mut activation).unwrap_err();
        let cause = std::mem::discriminant(failure.cause());
        assert!(!causes.contains(&cause), "{failure}");
        causes.push(cause);
        assert_final(&mut activation, &failure, after(t0, 11));
    }
}

#[test]
fn failures_name_their_state_and_their_cause() {
    let states = [
        (GuestRestoreState::Pending, "pending"),
        (GuestRestoreState::Activating, "activating"),
        (GuestRestoreState::Applied, "applied"),
        (GuestRestoreState::Ready, "ready"),
        (GuestRestoreState::Started, "started"),
        (GuestRestoreState::Failed, "failed"),
    ];
    for (state, text) in states {
        assert_eq!(state.to_string(), text);
    }
    let failure = |state, cause| GuestRestoreFailure {
        state,
        cause: Arc::new(cause),
    };
    let cases = [
        (
            failure(GuestRestoreState::Applied, GuestRestoreCause::Expired),
            "the activation failed in its applied state: the deadline passed before the \
             activation was ready",
        ),
        (
            failure(GuestRestoreState::Applied, GuestRestoreCause::Request),
            "the activation failed in its applied state: the host sent a request before the \
             activation was ready",
        ),
        (
            failure(GuestRestoreState::Pending, GuestRestoreCause::Missing),
            "the activation failed in its pending state: the restore packet carried no record",
        ),
        (
            failure(
                GuestRestoreState::Pending,
                GuestRestoreCause::Record(GuestRestoreError::Truncated),
            ),
            "the activation failed in its pending state: the restore packet's record is refused: \
             the record ends inside its version or one of its fields",
        ),
        (
            failure(GuestRestoreState::Pending, GuestRestoreCause::Cold),
            "the activation failed in its pending state: the restore packet's record is a cold \
             boot's, without a restore section",
        ),
        (
            failure(
                GuestRestoreState::Pending,
                GuestRestoreCause::Resources {
                    expected: ExpectedResources {
                        cpus: 4,
                        memory: 2 << 30,
                    },
                    actual: resources(),
                },
            ),
            "the activation failed in its pending state: the restore packet's record expects 4 \
             CPUs and 2147483648 bytes of memory, and the clone has 2 CPUs and 1073741824 bytes",
        ),
        (
            failure(
                GuestRestoreState::Activating,
                GuestRestoreCause::Failed("the root's assembly failed".into()),
            ),
            "the activation failed in its activating state: the record's application failed: \
             the root's assembly failed",
        ),
        (
            failure(GuestRestoreState::Ready, GuestRestoreCause::Accept),
            "the activation failed in its ready state: the activation took a record again",
        ),
        (
            failure(GuestRestoreState::Pending, GuestRestoreCause::Completion),
            "the activation failed in its pending state: an activation that did not run \
             completed",
        ),
        (
            failure(GuestRestoreState::Applied, GuestRestoreCause::Start),
            "the activation failed in its applied state: the sandbox started before the \
             activation was ready, or again",
        ),
    ];
    for (failure, text) in cases {
        assert_eq!(failure.to_string(), text);
        let source = match failure.cause() {
            GuestRestoreCause::Record(_) => {
                Some("the record ends inside its version or one of its fields")
            }
            GuestRestoreCause::Failed(_) => Some("the root's assembly failed"),
            _ => None,
        };
        assert_eq!(
            failure.source().map(|source| source.to_string()),
            source.map(str::to_owned)
        );
    }
}

#[test]
fn an_activation_debugs_its_state_resources_and_deadline() {
    let t0 = Instant::now();
    let activation = GuestRestore::with_resources(
        ExpectedResources {
            cpus: 4,
            memory: 2 << 30,
        },
        Some(t0),
        TIMEOUT,
        after(t0, 1),
    );
    assert_eq!(
        format!("{activation:?}"),
        format!(
            "GuestRestore {{ state: Pending, resources: ExpectedResources {{ cpus: 4, memory: \
             2147483648 }}, deadline: {:?}, .. }}",
            Some(after(t0, 10))
        )
    );
}
