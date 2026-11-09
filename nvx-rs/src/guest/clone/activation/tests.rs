//! Tests of a clone's activation, with a fake fence that counts how often it opened, records that
//! each test builds with `nvx::activation`, a fake clock, whose instants each test takes after a
//! base instant, and a fake adapter of a framing of the tests' own, whose transport delivers the
//! host's bytes when and as a test chooses, and whose output takes nothing.

use std::cell::Cell;
use std::io;
use std::num::NonZeroU64;
use std::rc::Rc;

use super::*;
use crate::activation::{
    Configuration, Identity, Image, MAX_RECORD_BYTES, MachineIdMode, OverlayMode, RestoreSection,
    StoreGeneration, TemplateReference, TransactionId, Upper, WorkloadView,
};

/// The number of online CPUs of the clone.
const CPUS: u32 = 2;
/// The memory size of the clone: eight online memory blocks of 128 MiB.
const MEMORY: u64 = 8 << 27;
/// How long the activation may take after the restore's acknowledgement.
const TIMEOUT: Duration = Duration::from_secs(10);
/// The transaction of the clone's restore.
const TRANSACTION: TransactionId = TransactionId::from_bytes([0x22; TransactionId::LEN]);
/// The kind of the host's request for Ready in the tests' framing.
const AWAIT: u8 = 1;
/// The kind of any other request of the host in the tests' framing.
const PING: u8 = 2;

/// Returns the activation, behind a fence, in a state that each such function chooses, that it
/// reached at an instant.
type Setup = fn(&FakeFence, Instant) -> Activation<FakeFence>;

/// Returns whether a failure's cause is the one that a test expects.
type Expected = fn(&ActivationCause) -> bool;

/// A fence that a restore handed over closed, which counts how often it opened, and which a test
/// shares with the activation that holds it.
#[derive(Clone, Default)]
struct FakeFence {
    /// How often the fence opened.
    opened: Rc<Cell<u32>>,
}

impl FakeFence {
    /// Returns how often the fence opened.
    fn opened(&self) -> u32 {
        self.opened.get()
    }
}

impl AdmissionFence for FakeFence {
    fn close(&mut self) -> io::Result<()> {
        unreachable!("an activation never closes the fence");
    }

    fn open(&mut self) {
        self.opened.set(self.opened.get() + 1);
    }
}

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
        image: Image::Erofs {
            binding: Identity::from_bytes([0x11; Identity::LEN]),
            uuid: None,
        },
        upper: Upper::Tmpfs,
        overlay: OverlayMode::UntrustedLowers,
        export: None,
        mappings: 0,
        network: None,
        workload: WorkloadView {
            hostname: hostname.to_vec(),
            machine_id: MachineIdMode::Image,
            identity: None,
        },
        resources,
    }
}

/// Returns the record of a clone's restore of [`TRANSACTION`], of a root named `hostname` that
/// expects `resources`.
fn clone_record(hostname: &[u8], resources: ExpectedResources) -> ActivationRecord {
    let restore = RestoreSection {
        transaction: TRANSACTION,
        template: TemplateReference {
            generation: StoreGeneration::new(NonZeroU64::MIN),
            key: Identity::from_bytes([0x44; Identity::LEN]),
        },
    };
    ActivationRecord::new(Some(restore), configuration(hostname, resources)).unwrap()
}

/// Returns the record of the clone's restore packet, which expects the clone's resources.
fn accepted() -> ActivationRecord {
    clone_record(b"clone", resources())
}

/// Returns the bytes of the record of the clone's restore packet.
fn record() -> Vec<u8> {
    accepted().encode()
}

/// Returns what Ready reports about the activation of the record of the clone's restore packet.
fn reported() -> ReadyActivation {
    ReadyActivation::of(&accepted())
}

/// Returns the activation, pending, of the clone behind `fence`, whose restore packet carried
/// `record`, if any, and whose repair acknowledged its restore at `acknowledged`.
fn restored(
    record: Option<&[u8]>,
    fence: &FakeFence,
    acknowledged: Instant,
) -> Activation<FakeFence> {
    Activation::with_resources(
        resources(),
        record,
        Some(acknowledged),
        fence.clone(),
        TIMEOUT,
        acknowledged,
    )
}

/// Returns the activation, pending, of the clone behind `fence`, whose restore packet carried the
/// record of [`record`], and whose repair acknowledged its restore at `acknowledged`.
fn pending(fence: &FakeFence, acknowledged: Instant) -> Activation<FakeFence> {
    let activation = restored(Some(&record()), fence, acknowledged);
    assert_eq!(activation.state(), ActivationState::Pending);
    activation
}

/// Returns the activation of [`pending`] once it accepted the record at `at`.
fn activating(fence: &FakeFence, at: Instant) -> Activation<FakeFence> {
    let mut activation = pending(fence, at);
    assert_eq!(*activation.accept(at).unwrap(), accepted());
    assert_eq!(activation.state(), ActivationState::Activating);
    activation
}

/// Returns the activation of [`activating`] once its application of the record completed at
/// `at`, before the host asked for Ready.
fn applied(fence: &FakeFence, at: Instant) -> Activation<FakeFence> {
    let mut activation = activating(fence, at);
    assert_eq!(activation.complete(at, Ok(())).unwrap(), reported());
    assert_eq!(activation.state(), ActivationState::Applied);
    assert_eq!(fence.opened(), 0);
    activation
}

/// Returns the activation of [`applied`] once the host asked for Ready at `at`.
fn ready(fence: &FakeFence, at: Instant) -> Activation<FakeFence> {
    let mut activation = applied(fence, at);
    let asked = activation.input(at, ActivationInput::Await);
    assert_eq!(asked.unwrap(), ActivationAction::Ready(reported()));
    assert_eq!(activation.state(), ActivationState::Ready);
    activation
}

/// Returns the activation of [`ready`] once the adapter's order started the sandbox.
fn started(fence: &FakeFence, at: Instant) -> Activation<FakeFence> {
    let mut activation = ready(fence, at);
    activation.start().unwrap();
    assert_eq!(activation.state(), ActivationState::Started);
    activation
}

/// Checks that `activation`, behind `fence`, failed finally with `failure` at `now`: it has no
/// deadline and no established activation, every later call returns the same failure, and the
/// fence opened `opened` times, as it had before the failure.
fn assert_final(
    activation: &mut Activation<FakeFence>,
    fence: &FakeFence,
    failure: &ActivationFailure,
    now: Instant,
    opened: u32,
) {
    assert_ne!(failure.state(), ActivationState::Failed);
    assert_eq!(activation.state(), ActivationState::Failed);
    assert_eq!(activation.deadline(), None);
    assert_eq!(activation.established(), None);
    let later = [
        activation.input(now, ActivationInput::Flow).unwrap_err(),
        activation.input(now, ActivationInput::Reset).unwrap_err(),
        activation.input(now, ActivationInput::Await).unwrap_err(),
        activation.input(now, ActivationInput::Request).unwrap_err(),
        activation.accept(now).unwrap_err(),
        activation.complete(now, Ok(())).unwrap_err(),
        activation.start().unwrap_err(),
        activation.tick(now).unwrap_err(),
    ];
    for later in later {
        assert_eq!(later.state(), failure.state());
        assert!(Arc::ptr_eq(&later.cause, &failure.cause));
    }
    assert_eq!(fence.opened(), opened);
}

#[test]
fn a_clone_activates_with_its_packet_s_record_and_opens_the_fence_once_ready() {
    let t0 = Instant::now();
    let fence = FakeFence::default();
    let mut activation = pending(&fence, t0);
    assert_eq!(activation.deadline(), Some(after(t0, 10)));
    assert_eq!(activation.established(), None);
    // The record of the restore packet, which the activation decoded, goes to the adapter, which
    // applies it, while the session takes the reset that the restore queued.
    let record = activation.accept(after(t0, 1)).unwrap();
    assert_eq!(*record, accepted());
    assert_eq!(activation.state(), ActivationState::Activating);
    let reset = activation.input(after(t0, 1), ActivationInput::Reset);
    assert_eq!(reset.unwrap(), ActivationAction::Acknowledge);
    let flow = activation.input(after(t0, 2), ActivationInput::Flow);
    assert_eq!(flow.unwrap(), ActivationAction::Continue);
    // The host's request for Ready comes while the application runs, and joins it.
    let asked = activation.input(after(t0, 2), ActivationInput::Await);
    assert_eq!(asked.unwrap(), ActivationAction::Join);
    assert_eq!(activation.deadline(), Some(after(t0, 10)));
    assert_eq!(activation.established(), None);
    assert_eq!(fence.opened(), 0);
    // Once the application succeeded, the fence opens, and Ready reports the activation, with the
    // record's transaction.
    let ready = activation.complete(after(t0, 3), Ok(())).unwrap();
    assert_eq!(ready, reported());
    assert_eq!(ready.transaction, Some(TRANSACTION));
    assert_eq!(fence.opened(), 1);
    assert_eq!(activation.state(), ActivationState::Ready);
    assert_eq!(activation.established(), Some(reported()));
    // No deadline holds a ready clone: the host's request for Ready, its mapping table, and its
    // start go to the adapter whenever they come.
    assert_eq!(activation.deadline(), None);
    activation.tick(after(t0, 3600)).unwrap();
    let asked = activation.input(after(t0, 3600), ActivationInput::Await);
    assert_eq!(asked.unwrap(), ActivationAction::Ready(reported()));
    let request = activation.input(after(t0, 3600), ActivationInput::Request);
    assert_eq!(request.unwrap(), ActivationAction::Serve);
    activation.start().unwrap();
    assert_eq!(activation.state(), ActivationState::Started);
    assert_eq!(activation.established(), Some(reported()));
    let request = activation.input(after(t0, 7200), ActivationInput::Request);
    assert_eq!(request.unwrap(), ActivationAction::Serve);
    let flow = activation.input(after(t0, 7200), ActivationInput::Flow);
    assert_eq!(flow.unwrap(), ActivationAction::Continue);
    assert_eq!(fence.opened(), 1);
}

#[test]
fn an_activation_that_applied_before_the_host_asked_is_ready_once_it_asks() {
    let t0 = Instant::now();
    // The application completed before the host's request for Ready: the activation is applied,
    // its fence closed, and its deadline holds, through a reset.
    let fence = FakeFence::default();
    let mut activation = applied(&fence, t0);
    assert_eq!(activation.deadline(), Some(after(t0, 10)));
    assert_eq!(activation.established(), None);
    activation.tick(after(t0, 9)).unwrap();
    let reset = activation.input(after(t0, 9), ActivationInput::Reset);
    assert_eq!(reset.unwrap(), ActivationAction::Acknowledge);
    assert_eq!(activation.state(), ActivationState::Applied);
    assert_eq!(fence.opened(), 0);
    // The host's request for Ready makes it ready, which opens the fence, and gets its Ready, as
    // does every later one, without activating again.
    let asked = activation.input(after(t0, 9), ActivationInput::Await);
    assert_eq!(asked.unwrap(), ActivationAction::Ready(reported()));
    assert_eq!(activation.state(), ActivationState::Ready);
    assert_eq!(activation.established(), Some(reported()));
    assert_eq!(activation.deadline(), None);
    assert_eq!(fence.opened(), 1);
    let asked = activation.input(after(t0, 60), ActivationInput::Await);
    assert_eq!(asked.unwrap(), ActivationAction::Ready(reported()));
    assert_eq!(fence.opened(), 1);
    // A clone whose host never asks fails at the deadline, with its fence closed.
    let fence = FakeFence::default();
    let mut activation = applied(&fence, t0);
    let failure = activation.tick(after(t0, 10)).unwrap_err();
    assert_eq!(failure.state(), ActivationState::Applied);
    assert!(matches!(failure.cause(), ActivationCause::Expired));
    assert_final(&mut activation, &fence, &failure, after(t0, 11), 0);
}

#[test]
fn the_host_s_requests_for_ready_join_the_activation_or_get_its_result() {
    let t0 = Instant::now();
    // Before the clone is ready, a request for Ready joins the activation, whatever its state:
    // the record came with the restore, so the host's request starts nothing.
    for activation in [pending, activating] {
        let fence = FakeFence::default();
        let mut activation = activation(&fence, t0);
        let state = activation.state();
        for _ in 0..3 {
            let asked = activation.input(after(t0, 1), ActivationInput::Await);
            assert_eq!(asked.unwrap(), ActivationAction::Join);
            assert_eq!(activation.state(), state);
        }
        assert_eq!(fence.opened(), 0);
    }
    // Once the clone is ready, or started, each request gets the established activation, in
    // every session, without activating again.
    for activation in [ready, started] {
        let fence = FakeFence::default();
        let mut activation = activation(&fence, t0);
        let state = activation.state();
        for session in 1..=3 {
            let now = after(t0, 60 * session);
            let reset = activation.input(now, ActivationInput::Reset);
            assert_eq!(reset.unwrap(), ActivationAction::Acknowledge);
            let asked = activation.input(now, ActivationInput::Await);
            assert_eq!(asked.unwrap(), ActivationAction::Ready(reported()));
            assert_eq!(activation.state(), state);
            assert_eq!(activation.established(), Some(reported()));
        }
        assert_eq!(fence.opened(), 1);
    }
}

#[test]
fn a_reset_abandons_only_the_session_s_requests_for_ready() {
    let t0 = Instant::now();
    // A reset, such as once the host's attachment ended, starts a later session, which the
    // adapter acknowledges: the activation goes on with the record of the restore packet, which
    // no session can change.
    for activation in [pending, activating, applied, ready, started] {
        let fence = FakeFence::default();
        let mut activation = activation(&fence, t0);
        let state = activation.state();
        let opened = fence.opened();
        for session in 1..=3 {
            let reset = activation.input(after(t0, session), ActivationInput::Reset);
            assert_eq!(reset.unwrap(), ActivationAction::Acknowledge);
            assert_eq!(activation.state(), state);
        }
        assert_eq!(fence.opened(), opened);
    }
    // A clone that the host reset while it activated completes its application all the same, and
    // the reset abandons the host's request for Ready of the earlier session: the activation is
    // applied, until the host asks again.
    let fence = FakeFence::default();
    let mut activation = activating(&fence, t0);
    let asked = activation.input(after(t0, 1), ActivationInput::Await);
    assert_eq!(asked.unwrap(), ActivationAction::Join);
    activation
        .input(after(t0, 1), ActivationInput::Reset)
        .unwrap();
    assert_eq!(
        activation.complete(after(t0, 2), Ok(())).unwrap(),
        reported()
    );
    assert_eq!(activation.state(), ActivationState::Applied);
    assert_eq!(fence.opened(), 0);
    let asked = activation.input(after(t0, 3), ActivationInput::Await);
    assert_eq!(asked.unwrap(), ActivationAction::Ready(reported()));
    assert_eq!(fence.opened(), 1);
}

#[test]
fn no_other_request_is_accepted_before_ready() {
    let t0 = Instant::now();
    // The host sends its mapping table, its start, and its probes only once Ready came, even
    // once the record applied.
    for activation in [pending, activating, applied] {
        let fence = FakeFence::default();
        let mut activation = activation(&fence, t0);
        let state = activation.state();
        let failure = activation
            .input(after(t0, 1), ActivationInput::Request)
            .unwrap_err();
        assert_eq!(failure.state(), state);
        assert!(matches!(failure.cause(), ActivationCause::Request));
        assert_final(&mut activation, &fence, &failure, after(t0, 2), 0);
    }
}

#[test]
fn the_packet_s_record_is_decoded_and_validated_before_any_side_effect() {
    let t0 = Instant::now();
    let mut long = record();
    long.resize(MAX_RECORD_BYTES + 1, 0);
    let mut version = record();
    version[0] = 2;
    let cold = ActivationRecord::new(None, configuration(b"clone", resources()))
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
    let cases: [(Option<Vec<u8>>, Expected); 8] = [
        (None, |cause| matches!(cause, ActivationCause::Missing)),
        (Some(Vec::new()), |cause| {
            matches!(cause, ActivationCause::Record(ActivationError::Truncated))
        }),
        (Some(long), |cause| {
            matches!(
                cause,
                ActivationCause::Record(ActivationError::TooLong(1025))
            )
        }),
        (Some(version), |cause| {
            matches!(cause, ActivationCause::Record(ActivationError::Version(2)))
        }),
        (Some(cold), |cause| matches!(cause, ActivationCause::Cold)),
        (Some(clone_record(b"clone", more_cpus).encode()), |cause| {
            matches!(
                cause,
                ActivationCause::Resources {
                    expected: ExpectedResources { cpus: 4, .. },
                    actual: ExpectedResources {
                        cpus: CPUS,
                        memory: MEMORY
                    },
                }
            )
        }),
        (
            Some(clone_record(b"clone", less_memory).encode()),
            |cause| matches!(cause, ActivationCause::Resources { expected, .. } if expected.memory == MEMORY - (128 << 20)),
        ),
        (
            Some(clone_record(b"clone", partial_block).encode()),
            |cause| matches!(cause, ActivationCause::Resources { expected, .. } if expected.memory == MEMORY + (64 << 20)),
        ),
    ];
    for (bytes, cause) in cases {
        let fence = FakeFence::default();
        let mut activation = restored(bytes.as_deref(), &fence, t0);
        let failure = activation.accept(after(t0, 1)).unwrap_err();
        assert_eq!(failure.state(), ActivationState::Pending);
        assert!(cause(failure.cause()), "{failure}");
        assert_final(&mut activation, &fence, &failure, after(t0, 2), 0);
    }
    // A valid record goes to the adapter exactly as it decodes, and opens nothing yet.
    let fence = FakeFence::default();
    let mut activation = pending(&fence, t0);
    let accepted = activation.accept(after(t0, 1)).unwrap();
    assert_eq!(*accepted, ActivationRecord::decode(&record()).unwrap());
    assert_eq!(fence.opened(), 0);
}

#[test]
fn a_failed_application_is_final_and_nothing_applies_the_record_again() {
    let t0 = Instant::now();
    let fence = FakeFence::default();
    let mut activation = activating(&fence, t0);
    let failure = activation
        .complete(after(t0, 1), Err("the root's assembly failed".into()))
        .unwrap_err();
    assert_eq!(failure.state(), ActivationState::Activating);
    let ActivationCause::Failed(error) = failure.cause() else {
        panic!("unexpected cause: {failure}");
    };
    assert_eq!(error.to_string(), "the root's assembly failed");
    assert_eq!(
        failure.source().unwrap().to_string(),
        "the root's assembly failed"
    );
    // Neither an acceptance nor a request for Ready applies the record again: each gets the
    // failure.
    assert_final(&mut activation, &fence, &failure, after(t0, 2), 0);
}

#[test]
fn the_deadline_starts_at_the_acknowledgement_and_nothing_extends_it() {
    let t0 = Instant::now();
    let fence = FakeFence::default();
    // The repair acknowledged the restore at `t0`, and the adapter made the activation later.
    let mut activation = Activation::with_resources(
        resources(),
        Some(&record()),
        Some(t0),
        fence.clone(),
        TIMEOUT,
        after(t0, 3),
    );
    assert_eq!(activation.deadline(), Some(after(t0, 10)));
    assert_eq!(*activation.accept(after(t0, 8)).unwrap(), accepted());
    let late = after(t0, 9) + Duration::from_millis(999);
    let asked = activation.input(late, ActivationInput::Await);
    assert_eq!(asked.unwrap(), ActivationAction::Join);
    activation.input(late, ActivationInput::Reset).unwrap();
    activation.tick(late).unwrap();
    assert_eq!(activation.deadline(), Some(after(t0, 10)));
    // At the deadline, the activation fails, however its application ends after it.
    let failure = activation.tick(after(t0, 10)).unwrap_err();
    assert_eq!(failure.state(), ActivationState::Activating);
    assert!(matches!(failure.cause(), ActivationCause::Expired));
    assert_final(&mut activation, &fence, &failure, after(t0, 11), 0);
    // An acceptance, an application that completes, a request for Ready of an applied
    // activation, and any input at the deadline come too late.
    let late: [(Setup, Failing); 5] = [
        (
            pending,
            Box::new(move |activation| activation.accept(after(t0, 10)).map(drop)),
        ),
        (
            activating,
            Box::new(move |activation| activation.complete(after(t0, 10), Ok(())).map(drop)),
        ),
        (
            activating,
            Box::new(move |activation| {
                let input = ActivationInput::Await;
                activation.input(after(t0, 10), input).map(drop)
            }),
        ),
        (
            applied,
            Box::new(move |activation| {
                let input = ActivationInput::Await;
                activation.input(after(t0, 10), input).map(drop)
            }),
        ),
        (
            pending,
            Box::new(move |activation| {
                let input = ActivationInput::Flow;
                activation.input(after(t0, 10), input).map(drop)
            }),
        ),
    ];
    for (activation, call) in late {
        let fence = FakeFence::default();
        let mut activation = activation(&fence, t0);
        let state = activation.state();
        let failure = call(&mut activation).unwrap_err();
        assert_eq!(failure.state(), state);
        assert!(matches!(failure.cause(), ActivationCause::Expired));
        assert_final(&mut activation, &fence, &failure, after(t0, 11), 0);
    }
}

#[test]
fn an_ungated_restore_s_deadline_starts_at_the_activation() {
    let t0 = Instant::now();
    let fence = FakeFence::default();
    // Without an acknowledgement, the deadline starts when the adapter makes the activation.
    let mut activation = Activation::with_resources(
        resources(),
        Some(&record()),
        None,
        fence.clone(),
        TIMEOUT,
        after(t0, 5),
    );
    assert_eq!(activation.deadline(), Some(after(t0, 15)));
    activation.tick(after(t0, 14)).unwrap();
    let failure = activation
        .input(after(t0, 15), ActivationInput::Reset)
        .unwrap_err();
    assert_eq!(failure.state(), ActivationState::Pending);
    assert!(matches!(failure.cause(), ActivationCause::Expired));
    assert_final(&mut activation, &fence, &failure, after(t0, 16), 0);
    // A deadline that no instant can hold never passes.
    let mut activation = Activation::with_resources(
        resources(),
        Some(&record()),
        Some(t0),
        fence.clone(),
        Duration::MAX,
        t0,
    );
    assert_eq!(activation.deadline(), None);
    activation.tick(after(t0, 1 << 30)).unwrap();
    let reset = activation.input(after(t0, 1 << 30), ActivationInput::Reset);
    assert_eq!(reset.unwrap(), ActivationAction::Acknowledge);
    assert_eq!(*activation.accept(after(t0, 1 << 30)).unwrap(), accepted());
}

#[test]
fn out_of_order_acceptances_completions_and_starts_are_final() {
    let t0 = Instant::now();
    // An activation accepts its record once, completes only while it runs, and the sandbox starts
    // only once it is ready, and once: the fence opens no sooner, and no more than once.
    let acceptances: [(Setup, u32); 4] = [(activating, 0), (applied, 0), (ready, 1), (started, 1)];
    for (activation, opened) in acceptances {
        let fence = FakeFence::default();
        let mut activation = activation(&fence, t0);
        let state = activation.state();
        let failure = activation.accept(after(t0, 1)).unwrap_err();
        assert_eq!(failure.state(), state);
        assert!(matches!(failure.cause(), ActivationCause::Completion));
        assert_final(&mut activation, &fence, &failure, after(t0, 2), opened);
    }
    let completions: [(Setup, u32); 4] = [(pending, 0), (applied, 0), (ready, 1), (started, 1)];
    for (activation, opened) in completions {
        let fence = FakeFence::default();
        let mut activation = activation(&fence, t0);
        let state = activation.state();
        let failure = activation.complete(after(t0, 1), Ok(())).unwrap_err();
        assert_eq!(failure.state(), state);
        assert!(matches!(failure.cause(), ActivationCause::Completion));
        assert_final(&mut activation, &fence, &failure, after(t0, 2), opened);
    }
    let starts: [(Setup, u32); 4] = [(pending, 0), (activating, 0), (applied, 0), (started, 1)];
    for (activation, opened) in starts {
        let fence = FakeFence::default();
        let mut activation = activation(&fence, t0);
        let state = activation.state();
        let failure = activation.start().unwrap_err();
        assert_eq!(failure.state(), state);
        assert!(matches!(failure.cause(), ActivationCause::Start));
        assert_final(&mut activation, &fence, &failure, after(t0, 2), opened);
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
    Ready(ReadyActivation),
}

/// An adapter of the tests' framing, which drives an activation as an adapter's loop does: it
/// takes the host's bytes as its transport delivers them, passes the activation each message
/// that they complete, and flow control for each byte that completes none, counts the host's
/// requests for Ready that joined the activation in the current session, which it answers once
/// the test completed the activation, and queues its replies for an output that takes nothing,
/// as a host that reads nothing leaves it, and which it never waits for.
struct Adapter {
    /// The activation.
    activation: Activation<FakeFence>,
    /// The bytes of the host's message in progress.
    message: Vec<u8>,
    /// The replies that wait for the output.
    output: Vec<Reply>,
    /// The host's requests for Ready of the current session that joined the activation.
    joined: usize,
}

impl Adapter {
    /// Returns the adapter that drives `activation`.
    fn new(activation: Activation<FakeFence>) -> Self {
        Self {
            activation,
            message: Vec::new(),
            output: Vec::new(),
            joined: 0,
        }
    }

    /// Takes the session's reset at `now`.
    fn reset(&mut self, now: Instant) -> Result<(), ActivationFailure> {
        let action = self.activation.input(now, ActivationInput::Reset)?;
        self.act(action);
        Ok(())
    }

    /// Takes `bytes` of the host's messages, which the transport delivered at `now`, one at a
    /// time.
    fn receive(&mut self, now: Instant, bytes: &[u8]) -> Result<(), ActivationFailure> {
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
                    [AWAIT, _, _] => ActivationInput::Await,
                    _ => ActivationInput::Request,
                };
                self.activation.input(now, input)?
            } else {
                self.activation.input(now, ActivationInput::Flow)?
            };
            self.act(action);
        }
        Ok(())
    }

    /// Completes the activation at `now` with an application that succeeded, and answers each
    /// request for Ready that joined it.
    fn complete(&mut self, now: Instant) -> Result<(), ActivationFailure> {
        let ready = self.activation.complete(now, Ok(()))?;
        for _ in 0..std::mem::take(&mut self.joined) {
            self.output.push(Reply::Ready(ready));
        }
        Ok(())
    }

    /// Does what `action` says.
    fn act(&mut self, action: ActivationAction) {
        match action {
            ActivationAction::Acknowledge => {
                self.joined = 0;
                self.output.push(Reply::Acknowledgement);
            }
            ActivationAction::Continue | ActivationAction::Serve => {}
            ActivationAction::Join => self.joined += 1,
            ActivationAction::Ready(ready) => self.output.push(Reply::Ready(ready)),
        }
    }
}

#[test]
fn the_deadline_stays_live_under_trickled_input_and_blocked_output() {
    let t0 = Instant::now();
    let asked = message(AWAIT, b"");
    let ping = message(PING, b"0123456789");
    // A host that trickles a message, a byte a second, while the record's application runs past
    // the deadline gets no Ready: the byte at the deadline fails the activation, while the output
    // took nothing.
    let fence = FakeFence::default();
    let mut adapter = Adapter::new(activating(&fence, t0));
    adapter.reset(t0).unwrap();
    let mut failure = None;
    for (second, byte) in (1..).zip(ping.iter().cycle()) {
        if let Err(error) = adapter.receive(after(t0, second), &[*byte]) {
            failure = Some((second, error));
            break;
        }
    }
    let (second, failure) = failure.unwrap();
    assert_eq!(second, 10);
    assert_eq!(failure.state(), ActivationState::Activating);
    assert!(matches!(failure.cause(), ActivationCause::Expired));
    assert_eq!(adapter.output, [Reply::Acknowledgement]);
    assert_final(&mut adapter.activation, &fence, &failure, after(t0, 11), 0);
    // A host that sends nothing leaves the adapter only its wake at the deadline, which fails an
    // activation whose application did not complete.
    let fence = FakeFence::default();
    let mut adapter = Adapter::new(activating(&fence, t0));
    adapter.reset(after(t0, 1)).unwrap();
    let deadline = adapter.activation.deadline().unwrap();
    assert_eq!(deadline, after(t0, 10));
    adapter
        .activation
        .tick(deadline - Duration::from_millis(1))
        .unwrap();
    let failure = adapter.activation.tick(deadline).unwrap_err();
    assert_eq!(failure.state(), ActivationState::Activating);
    assert!(matches!(failure.cause(), ActivationCause::Expired));
    assert_final(&mut adapter.activation, &fence, &failure, after(t0, 11), 0);
    // A host whose request for Ready arrives while the record's application runs gets Ready once
    // the application completed, and only in the session in which it asked: a reset abandons the
    // requests of the earlier session, and a host that asks again in the later one gets one
    // Ready.
    let fence = FakeFence::default();
    let mut adapter = Adapter::new(activating(&fence, t0));
    adapter.reset(t0).unwrap();
    for (second, byte) in (1..).zip(&asked) {
        adapter.receive(after(t0, second), &[*byte]).unwrap();
    }
    assert_eq!(adapter.joined, 1);
    adapter.reset(after(t0, 4)).unwrap();
    adapter.receive(after(t0, 5), &asked).unwrap();
    adapter.receive(after(t0, 5), &asked).unwrap();
    adapter.complete(after(t0, 6)).unwrap();
    assert_eq!(
        adapter.output,
        [
            Reply::Acknowledgement,
            Reply::Acknowledgement,
            Reply::Ready(reported()),
            Reply::Ready(reported())
        ]
    );
    assert_eq!(fence.opened(), 1);
    // Once ready, a request for Ready gets its answer at once.
    adapter.receive(after(t0, 7), &asked).unwrap();
    assert_eq!(adapter.output.last(), Some(&Reply::Ready(reported())));
    // A host that asks for Ready only once the application completed gets it then, which opens
    // the fence.
    let fence = FakeFence::default();
    let mut adapter = Adapter::new(activating(&fence, t0));
    adapter.reset(t0).unwrap();
    adapter.complete(after(t0, 1)).unwrap();
    assert_eq!(adapter.output, [Reply::Acknowledgement]);
    assert_eq!(fence.opened(), 0);
    adapter.receive(after(t0, 2), &asked).unwrap();
    assert_eq!(
        adapter.output,
        [Reply::Acknowledgement, Reply::Ready(reported())]
    );
    assert_eq!(fence.opened(), 1);
    // Another message of the framing, whole, while the record's application runs fails the
    // activation.
    let fence = FakeFence::default();
    let mut adapter = Adapter::new(activating(&fence, t0));
    let failure = adapter.receive(after(t0, 1), &ping).unwrap_err();
    assert_eq!(failure.state(), ActivationState::Activating);
    assert!(matches!(failure.cause(), ActivationCause::Request));
    assert_final(&mut adapter.activation, &fence, &failure, after(t0, 2), 0);
}

/// A call that fails an activation.
type Failing = Box<dyn Fn(&mut Activation<FakeFence>) -> Result<(), ActivationFailure>>;

#[test]
fn strict_failures_are_returned_and_never_power_off() {
    let t0 = Instant::now();
    let cold = ActivationRecord::new(None, configuration(b"clone", resources()))
        .unwrap()
        .encode();
    let more_cpus = ExpectedResources {
        cpus: CPUS + 1,
        memory: MEMORY,
    };
    let unexpected = clone_record(b"clone", more_cpus).encode();
    // Each failure, in a state in which it comes: the activation returns it, keeps it, and changes
    // nothing else. It has no system through which it could power the VM off: the adapter reports
    // the failure, and ends the VM.
    let records: [Option<Vec<u8>>; 4] = [None, Some(Vec::new()), Some(cold), Some(unexpected)];
    let mut causes = Vec::new();
    for record in records {
        let fence = FakeFence::default();
        let mut activation = restored(record.as_deref(), &fence, t0);
        let failure = activation.accept(after(t0, 1)).unwrap_err();
        let cause = std::mem::discriminant(failure.cause());
        assert!(!causes.contains(&cause), "{failure}");
        causes.push(cause);
        assert_final(&mut activation, &fence, &failure, after(t0, 11), 0);
    }
    let failures: [(Setup, Failing); 5] = [
        (
            activating,
            Box::new(move |activation| activation.tick(after(t0, 10))),
        ),
        (
            activating,
            Box::new(move |activation| {
                activation
                    .input(after(t0, 1), ActivationInput::Request)
                    .map(drop)
            }),
        ),
        (
            activating,
            Box::new(move |activation| {
                activation
                    .complete(after(t0, 1), Err("failed".into()))
                    .map(drop)
            }),
        ),
        (
            pending,
            Box::new(move |activation| activation.complete(after(t0, 1), Ok(())).map(drop)),
        ),
        (activating, Box::new(|activation| activation.start())),
    ];
    for (activation, fail) in failures {
        let fence = FakeFence::default();
        let mut activation = activation(&fence, t0);
        let opened = fence.opened();
        let failure = fail(&mut activation).unwrap_err();
        let cause = std::mem::discriminant(failure.cause());
        assert!(!causes.contains(&cause), "{failure}");
        causes.push(cause);
        assert_final(&mut activation, &fence, &failure, after(t0, 11), opened);
    }
}

#[test]
fn failures_name_their_state_and_their_cause() {
    let states = [
        (ActivationState::Pending, "pending"),
        (ActivationState::Activating, "activating"),
        (ActivationState::Applied, "applied"),
        (ActivationState::Ready, "ready"),
        (ActivationState::Started, "started"),
        (ActivationState::Failed, "failed"),
    ];
    for (state, text) in states {
        assert_eq!(state.to_string(), text);
    }
    let failure = |state, cause| ActivationFailure {
        state,
        cause: Arc::new(cause),
    };
    let cases = [
        (
            failure(ActivationState::Pending, ActivationCause::Expired),
            "the activation failed in its pending state: the deadline passed before the \
             activation was ready",
        ),
        (
            failure(ActivationState::Pending, ActivationCause::Missing),
            "the activation failed in its pending state: the clone's restore packet carried no \
             activation record",
        ),
        (
            failure(ActivationState::Activating, ActivationCause::Request),
            "the activation failed in its activating state: the host sent a request before the \
             activation was ready",
        ),
        (
            failure(ActivationState::Applied, ActivationCause::Expired),
            "the activation failed in its applied state: the deadline passed before the \
             activation was ready",
        ),
        (
            failure(
                ActivationState::Pending,
                ActivationCause::Record(ActivationError::Truncated),
            ),
            "the activation failed in its pending state: the restore packet's activation record \
             is refused: the record ends inside its version or one of its fields",
        ),
        (
            failure(ActivationState::Pending, ActivationCause::Cold),
            "the activation failed in its pending state: the restore packet's activation record \
             is a cold boot's, without a restore section",
        ),
        (
            failure(
                ActivationState::Pending,
                ActivationCause::Resources {
                    expected: ExpectedResources {
                        cpus: 4,
                        memory: 2 << 30,
                    },
                    actual: resources(),
                },
            ),
            "the activation failed in its pending state: the activation record expects 4 CPUs \
             and 2147483648 bytes of memory, and the clone has 2 CPUs and 1073741824 bytes",
        ),
        (
            failure(
                ActivationState::Activating,
                ActivationCause::Failed("the root's assembly failed".into()),
            ),
            "the activation failed in its activating state: the record's application failed: \
             the root's assembly failed",
        ),
        (
            failure(ActivationState::Ready, ActivationCause::Completion),
            "the activation failed in its ready state: the activation's record was accepted \
             again, or an activation that did not run completed",
        ),
        (
            failure(ActivationState::Activating, ActivationCause::Start),
            "the activation failed in its activating state: the sandbox started before the \
             activation was ready, or again",
        ),
    ];
    for (failure, text) in cases {
        assert_eq!(failure.to_string(), text);
        let source = match failure.cause() {
            ActivationCause::Record(_) => {
                Some("the record ends inside its version or one of its fields")
            }
            ActivationCause::Failed(_) => Some("the root's assembly failed"),
            _ => None,
        };
        assert_eq!(
            failure.source().map(|source| source.to_string()),
            source.map(str::to_owned)
        );
    }
}

#[test]
fn an_activation_debugs_its_state_resources_and_deadline_without_its_fence_or_record() {
    let t0 = Instant::now();
    let activation = Activation::with_resources(
        ExpectedResources {
            cpus: 4,
            memory: 2 << 30,
        },
        Some(&record()),
        Some(t0),
        FakeFence::default(),
        TIMEOUT,
        after(t0, 1),
    );
    assert_eq!(
        format!("{activation:?}"),
        format!(
            "Activation {{ state: Pending, resources: ExpectedResources {{ cpus: 4, memory: \
             2147483648 }}, deadline: {:?}, .. }}",
            Some(after(t0, 10))
        )
    );
}
