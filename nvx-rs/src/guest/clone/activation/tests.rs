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
    StoreGeneration, TemplateReference, Upper, WorkloadView,
};

/// The number of online CPUs of the clone.
const CPUS: u32 = 2;
/// The memory size of the clone: eight online memory blocks of 128 MiB.
const MEMORY: u64 = 8 << 27;
/// How long the activation may take after the restore's acknowledgement.
const TIMEOUT: Duration = Duration::from_secs(10);
/// The transaction of the host's Activate.
const TRANSACTION: TransactionId = TransactionId::from_bytes([0x22; TransactionId::LEN]);
/// The transaction of another restore.
const OTHER_TRANSACTION: TransactionId = TransactionId::from_bytes([0x33; TransactionId::LEN]);
/// The kind of the host's Activate in the tests' framing.
const ACTIVATE: u8 = 1;
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

/// Returns the record of a clone's restore of `transaction`, of a root named `hostname` that
/// expects `resources`.
fn clone_record(
    transaction: TransactionId,
    hostname: &[u8],
    resources: ExpectedResources,
) -> ActivationRecord {
    let restore = RestoreSection {
        transaction,
        template: TemplateReference {
            generation: StoreGeneration::new(NonZeroU64::MIN),
            key: Identity::from_bytes([0x44; Identity::LEN]),
        },
    };
    ActivationRecord::new(Some(restore), configuration(hostname, resources)).unwrap()
}

/// Returns the record of the host's Activate: the clone's restore of [`TRANSACTION`], which
/// expects the clone's resources.
fn accepted() -> ActivationRecord {
    clone_record(TRANSACTION, b"clone", resources())
}

/// Returns the bytes of the record of the host's Activate.
fn record() -> Vec<u8> {
    accepted().encode()
}

/// Returns what Ready reports about the activation of the host's Activate.
fn reported() -> ReadyActivation {
    ReadyActivation::of(&accepted())
}

/// Returns the activation, in its transport bootstrap, of the clone behind `fence`, whose repair
/// acknowledged its restore at `acknowledged`.
fn bootstrap(fence: &FakeFence, acknowledged: Instant) -> Activation<FakeFence> {
    Activation::with_resources(
        resources(),
        Some(acknowledged),
        fence.clone(),
        TIMEOUT,
        acknowledged,
    )
}

/// Returns the activation of [`bootstrap`] once the session's reset at `at` restricted it.
fn restricted(fence: &FakeFence, at: Instant) -> Activation<FakeFence> {
    let mut activation = bootstrap(fence, at);
    let action = activation.input(at, ActivationInput::Reset).unwrap();
    assert_eq!(action, ActivationAction::Acknowledge);
    assert_eq!(activation.state(), ActivationState::Restricted);
    activation
}

/// Returns the activation of [`restricted`] once it accepted the host's Activate at `at`.
fn activating(fence: &FakeFence, at: Instant) -> Activation<FakeFence> {
    let mut activation = restricted(fence, at);
    let action = activation
        .input(at, ActivationInput::Activate(&record()))
        .unwrap();
    assert_eq!(action, ActivationAction::Apply(Box::new(accepted())));
    assert_eq!(activation.state(), ActivationState::Activating);
    activation
}

/// Returns the activation of [`activating`] once its application of the record completed at
/// `at`.
fn ready(fence: &FakeFence, at: Instant) -> Activation<FakeFence> {
    let mut activation = activating(fence, at);
    assert_eq!(activation.complete(at, Ok(())).unwrap(), reported());
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
    let record = record();
    let later = [
        activation.input(now, ActivationInput::Flow).unwrap_err(),
        activation.input(now, ActivationInput::Reset).unwrap_err(),
        activation
            .input(now, ActivationInput::Activate(&record))
            .unwrap_err(),
        activation.input(now, ActivationInput::Request).unwrap_err(),
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
fn a_clone_bootstraps_takes_one_activate_and_opens_the_fence_once_ready() {
    let t0 = Instant::now();
    let fence = FakeFence::default();
    let mut activation = bootstrap(&fence, t0);
    assert_eq!(activation.state(), ActivationState::Bootstrap);
    assert_eq!(activation.deadline(), Some(after(t0, 10)));
    // Bytes of the session's reset arrive before it completes, and then the reset.
    let flow = activation.input(after(t0, 1), ActivationInput::Flow);
    assert_eq!(flow.unwrap(), ActivationAction::Continue);
    let reset = activation.input(after(t0, 1), ActivationInput::Reset);
    assert_eq!(reset.unwrap(), ActivationAction::Acknowledge);
    assert_eq!(activation.state(), ActivationState::Restricted);
    let flow = activation.input(after(t0, 2), ActivationInput::Flow);
    assert_eq!(flow.unwrap(), ActivationAction::Continue);
    // The Activate's record, which the activation decoded, goes to the adapter, which applies it.
    let action = activation.input(after(t0, 2), ActivationInput::Activate(&record()));
    assert_eq!(
        action.unwrap(),
        ActivationAction::Apply(Box::new(accepted()))
    );
    assert_eq!(activation.state(), ActivationState::Activating);
    assert_eq!(activation.deadline(), Some(after(t0, 10)));
    assert_eq!(activation.established(), None);
    assert_eq!(fence.opened(), 0);
    let flow = activation.input(after(t0, 3), ActivationInput::Flow);
    assert_eq!(flow.unwrap(), ActivationAction::Continue);
    // Once the application succeeded, the fence opens, and Ready reports the activation, with the
    // record's transaction.
    let ready = activation.complete(after(t0, 4), Ok(())).unwrap();
    assert_eq!(ready, reported());
    assert_eq!(ready.transaction, Some(TRANSACTION));
    assert_eq!(fence.opened(), 1);
    assert_eq!(activation.state(), ActivationState::Ready);
    assert_eq!(activation.established(), Some(reported()));
    // No deadline holds a ready clone: the host's mapping table and its start go to the adapter's
    // order, whenever they come.
    assert_eq!(activation.deadline(), None);
    activation.tick(after(t0, 3600)).unwrap();
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
fn no_request_is_accepted_before_activate() {
    let t0 = Instant::now();
    let record = record();
    // Before the session's bootstrap, the host can send nothing, Activate included; while the
    // session is restricted, and while the activation runs, nothing but the Activate.
    let cases: [(Setup, ActivationInput<'_>); 4] = [
        (bootstrap, ActivationInput::Request),
        (bootstrap, ActivationInput::Activate(&record)),
        (restricted, ActivationInput::Request),
        (activating, ActivationInput::Request),
    ];
    for (activation, input) in cases {
        let fence = FakeFence::default();
        let mut activation = activation(&fence, t0);
        let state = activation.state();
        let failure = activation.input(after(t0, 1), input).unwrap_err();
        assert_eq!(failure.state(), state, "{input:?}");
        assert!(matches!(failure.cause(), ActivationCause::Request));
        assert_final(&mut activation, &fence, &failure, after(t0, 2), 0);
    }
}

#[test]
fn a_reset_before_ready_is_final_and_one_after_it_starts_a_later_session() {
    let t0 = Instant::now();
    // Only the reset that the restore queued bootstraps the session: another, such as once the
    // host's attachment ended, fails the activation before it is ready.
    for activation in [restricted, activating] {
        let fence = FakeFence::default();
        let mut activation = activation(&fence, t0);
        let state = activation.state();
        let failure = activation
            .input(after(t0, 1), ActivationInput::Reset)
            .unwrap_err();
        assert_eq!(failure.state(), state);
        assert!(matches!(failure.cause(), ActivationCause::Reset));
        assert_final(&mut activation, &fence, &failure, after(t0, 1), 0);
    }
    // Once the clone is ready, or started, a reset starts a later session, which gets the
    // established activation and transaction without activating again.
    for activation in [ready, started] {
        let fence = FakeFence::default();
        let mut activation = activation(&fence, t0);
        let state = activation.state();
        for session in 1..=3 {
            let now = after(t0, 60 * session);
            let reset = activation.input(now, ActivationInput::Reset);
            assert_eq!(reset.unwrap(), ActivationAction::Acknowledge);
            assert_eq!(activation.state(), state);
            assert_eq!(activation.established(), Some(reported()));
            let retry = activation.input(now, ActivationInput::Activate(&record()));
            assert_eq!(retry.unwrap(), ActivationAction::Ready(reported()));
            let request = activation.input(now, ActivationInput::Request);
            assert_eq!(request.unwrap(), ActivationAction::Serve);
        }
        assert_eq!(fence.opened(), 1);
    }
}

#[test]
fn retries_join_the_running_activation_and_get_its_cached_result() {
    let t0 = Instant::now();
    let fence = FakeFence::default();
    let mut activation = activating(&fence, t0);
    // A retry of the same transaction, with a byte-equal record, joins the activation that runs,
    // whose deadline stays where it was.
    for second in 1..=3 {
        let retry = activation.input(after(t0, second), ActivationInput::Activate(&record()));
        assert_eq!(retry.unwrap(), ActivationAction::Join);
        assert_eq!(activation.state(), ActivationState::Activating);
        assert_eq!(activation.deadline(), Some(after(t0, 10)));
    }
    assert_eq!(fence.opened(), 0);
    assert_eq!(
        activation.complete(after(t0, 4), Ok(())).unwrap(),
        reported()
    );
    // Afterwards, one gets the cached result, before and after the start, without applying the
    // record again or opening the fence again.
    let retry = activation.input(after(t0, 5), ActivationInput::Activate(&record()));
    assert_eq!(retry.unwrap(), ActivationAction::Ready(reported()));
    activation.start().unwrap();
    let retry = activation.input(after(t0, 6), ActivationInput::Activate(&record()));
    assert_eq!(retry.unwrap(), ActivationAction::Ready(reported()));
    assert_eq!(activation.state(), ActivationState::Started);
    assert_eq!(fence.opened(), 1);
}

#[test]
fn a_changed_record_or_a_new_transaction_is_final() {
    let t0 = Instant::now();
    let renamed = clone_record(TRANSACTION, b"renamed", resources()).encode();
    let other = clone_record(OTHER_TRANSACTION, b"clone", resources()).encode();
    let cold = ActivationRecord::new(None, configuration(b"clone", resources()))
        .unwrap()
        .encode();
    let whole = record();
    let truncated = &whole[..whole.len() - 1];
    let retries: [(&[u8], Expected); 4] = [
        (&renamed, |cause| matches!(cause, ActivationCause::Changed)),
        (&other, |cause| {
            matches!(
                cause,
                ActivationCause::Transaction {
                    accepted: TRANSACTION,
                    offered: OTHER_TRANSACTION,
                }
            )
        }),
        (&cold, |cause| matches!(cause, ActivationCause::Cold)),
        (truncated, |cause| {
            matches!(cause, ActivationCause::Record(ActivationError::Truncated))
        }),
    ];
    // While the activation runs, and once it is ready, or started, whose fence stays open.
    let states: [(Setup, u32); 3] = [(activating, 0), (ready, 1), (started, 1)];
    for (activation, opened) in states {
        for (retry, cause) in retries {
            let fence = FakeFence::default();
            let mut activation = activation(&fence, t0);
            let state = activation.state();
            let failure = activation
                .input(after(t0, 1), ActivationInput::Activate(retry))
                .unwrap_err();
            assert_eq!(failure.state(), state);
            assert!(cause(failure.cause()), "{state}: {failure}");
            assert_final(&mut activation, &fence, &failure, after(t0, 2), opened);
        }
    }
}

#[test]
fn an_activate_too_long_to_read_fails_as_a_record_that_long() {
    let t0 = Instant::now();
    // In the transport bootstrap, as any Activate.
    let fence = FakeFence::default();
    let mut activation = bootstrap(&fence, t0);
    let failure = activation
        .input(after(t0, 1), ActivationInput::TooLong(MAX_RECORD_BYTES + 1))
        .unwrap_err();
    assert_eq!(failure.state(), ActivationState::Bootstrap);
    assert!(matches!(failure.cause(), ActivationCause::Request));
    assert_final(&mut activation, &fence, &failure, after(t0, 2), 0);
    // From then on, as an Activate whose record has that many bytes, which no record has: while
    // restricted, while the activation runs, and once it is ready, or started, whose fence stays
    // open.
    let states: [(Setup, u32); 4] = [(restricted, 0), (activating, 0), (ready, 1), (started, 1)];
    for (activation, opened) in states {
        for length in [MAX_RECORD_BYTES + 1, usize::MAX] {
            let fence = FakeFence::default();
            let mut activation = activation(&fence, t0);
            let state = activation.state();
            let failure = activation
                .input(after(t0, 1), ActivationInput::TooLong(length))
                .unwrap_err();
            assert_eq!(failure.state(), state);
            assert!(
                matches!(
                    failure.cause(),
                    ActivationCause::Record(ActivationError::TooLong(long)) if *long == length
                ),
                "{state}: {failure}"
            );
            assert_final(&mut activation, &fence, &failure, after(t0, 2), opened);
        }
    }
    // Past the deadline, before the clone is ready, it fails as every input does.
    let fence = FakeFence::default();
    let mut activation = restricted(&fence, t0);
    let failure = activation
        .input(
            after(t0, 10),
            ActivationInput::TooLong(MAX_RECORD_BYTES + 1),
        )
        .unwrap_err();
    assert!(matches!(failure.cause(), ActivationCause::Expired));
    assert_final(&mut activation, &fence, &failure, after(t0, 11), 0);
}

#[test]
fn a_failed_application_is_final_and_no_retry_applies_the_record_again() {
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
    // A byte-equal retry neither joins nor applies the record again: it gets the failure.
    assert_final(&mut activation, &fence, &failure, after(t0, 2), 0);
}

#[test]
fn an_activate_is_decoded_and_validated_before_any_side_effect() {
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
    let cases: [(Vec<u8>, Expected); 8] = [
        (Vec::new(), |cause| {
            matches!(cause, ActivationCause::Record(ActivationError::Truncated))
        }),
        (long, |cause| {
            matches!(
                cause,
                ActivationCause::Record(ActivationError::TooLong(1025))
            )
        }),
        (version, |cause| {
            matches!(cause, ActivationCause::Record(ActivationError::Version(2)))
        }),
        (cold, |cause| matches!(cause, ActivationCause::Cold)),
        (
            clone_record(TRANSACTION, b"clone", more_cpus).encode(),
            |cause| {
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
            },
        ),
        (
            clone_record(TRANSACTION, b"clone", less_memory).encode(),
            |cause| matches!(cause, ActivationCause::Resources { expected, .. } if expected.memory == MEMORY - (128 << 20)),
        ),
        (
            clone_record(TRANSACTION, b"clone", partial_block).encode(),
            |cause| matches!(cause, ActivationCause::Resources { expected, .. } if expected.memory == MEMORY + (64 << 20)),
        ),
        (
            clone_record(OTHER_TRANSACTION, b"clone", more_cpus).encode(),
            |cause| matches!(cause, ActivationCause::Resources { .. }),
        ),
    ];
    for (bytes, cause) in cases {
        let fence = FakeFence::default();
        let mut activation = restricted(&fence, t0);
        let failure = activation
            .input(after(t0, 1), ActivationInput::Activate(&bytes))
            .unwrap_err();
        assert_eq!(failure.state(), ActivationState::Restricted);
        assert!(cause(failure.cause()), "{failure}");
        assert_final(&mut activation, &fence, &failure, after(t0, 2), 0);
    }
    // A valid record goes to the adapter exactly as it decodes, and opens nothing yet.
    let fence = FakeFence::default();
    let mut activation = restricted(&fence, t0);
    let action = activation.input(after(t0, 1), ActivationInput::Activate(&record()));
    assert_eq!(
        action.unwrap(),
        ActivationAction::Apply(Box::new(ActivationRecord::decode(&record()).unwrap()))
    );
    assert_eq!(fence.opened(), 0);
}

#[test]
fn the_deadline_starts_at_the_acknowledgement_and_retries_never_extend_it() {
    let t0 = Instant::now();
    let fence = FakeFence::default();
    // The repair acknowledged the restore at `t0`, and the adapter made the activation later.
    let mut activation =
        Activation::with_resources(resources(), Some(t0), fence.clone(), TIMEOUT, after(t0, 3));
    assert_eq!(activation.deadline(), Some(after(t0, 10)));
    activation
        .input(after(t0, 8), ActivationInput::Reset)
        .unwrap();
    let action = activation.input(after(t0, 9), ActivationInput::Activate(&record()));
    assert_eq!(
        action.unwrap(),
        ActivationAction::Apply(Box::new(accepted()))
    );
    let late = after(t0, 9) + Duration::from_millis(999);
    let retry = activation.input(late, ActivationInput::Activate(&record()));
    assert_eq!(retry.unwrap(), ActivationAction::Join);
    activation.tick(late).unwrap();
    assert_eq!(activation.deadline(), Some(after(t0, 10)));
    // At the deadline, the activation fails, however its application ends after it.
    let failure = activation.tick(after(t0, 10)).unwrap_err();
    assert_eq!(failure.state(), ActivationState::Activating);
    assert!(matches!(failure.cause(), ActivationCause::Expired));
    assert_final(&mut activation, &fence, &failure, after(t0, 11), 0);
    // An application that completes at the deadline completes too late, as does any input.
    for complete in [true, false] {
        let fence = FakeFence::default();
        let mut activation = activating(&fence, t0);
        let failure = if complete {
            activation.complete(after(t0, 10), Ok(())).unwrap_err()
        } else {
            let retry = ActivationInput::Activate(&record());
            activation.input(after(t0, 10), retry).unwrap_err()
        };
        assert_eq!(failure.state(), ActivationState::Activating);
        assert!(matches!(failure.cause(), ActivationCause::Expired));
        assert_final(&mut activation, &fence, &failure, after(t0, 11), 0);
    }
}

#[test]
fn an_ungated_restore_s_deadline_starts_at_the_activation() {
    let t0 = Instant::now();
    let fence = FakeFence::default();
    // Without an acknowledgement, the deadline starts when the adapter makes the activation.
    let mut activation =
        Activation::with_resources(resources(), None, fence.clone(), TIMEOUT, after(t0, 5));
    assert_eq!(activation.deadline(), Some(after(t0, 15)));
    activation.tick(after(t0, 14)).unwrap();
    let failure = activation
        .input(after(t0, 15), ActivationInput::Reset)
        .unwrap_err();
    assert_eq!(failure.state(), ActivationState::Bootstrap);
    assert!(matches!(failure.cause(), ActivationCause::Expired));
    assert_final(&mut activation, &fence, &failure, after(t0, 16), 0);
    // A deadline that no instant can hold never passes.
    let mut activation =
        Activation::with_resources(resources(), Some(t0), fence.clone(), Duration::MAX, t0);
    assert_eq!(activation.deadline(), None);
    activation.tick(after(t0, 1 << 30)).unwrap();
    let reset = activation.input(after(t0, 1 << 30), ActivationInput::Reset);
    assert_eq!(reset.unwrap(), ActivationAction::Acknowledge);
}

#[test]
fn out_of_order_completions_and_starts_are_final() {
    let t0 = Instant::now();
    // An activation completes only while it runs, and the sandbox starts only once it is ready,
    // and once: the fence opens no sooner, and no more than once.
    let completions: [(Setup, u32); 4] =
        [(bootstrap, 0), (restricted, 0), (ready, 1), (started, 1)];
    for (activation, opened) in completions {
        let fence = FakeFence::default();
        let mut activation = activation(&fence, t0);
        let state = activation.state();
        let failure = activation.complete(after(t0, 1), Ok(())).unwrap_err();
        assert_eq!(failure.state(), state);
        assert!(matches!(failure.cause(), ActivationCause::Completion));
        assert_final(&mut activation, &fence, &failure, after(t0, 2), opened);
    }
    let starts: [(Setup, u32); 4] = [
        (bootstrap, 0),
        (restricted, 0),
        (activating, 0),
        (started, 1),
    ];
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
/// that they complete, and flow control for each byte that completes none, keeps the records
/// that the activation accepts, whose application the test completes when it chooses, and queues
/// its replies for an output that takes nothing, as a host that reads nothing leaves it, and
/// which it never waits for.
struct Adapter {
    /// The activation.
    activation: Activation<FakeFence>,
    /// The bytes of the host's message in progress.
    message: Vec<u8>,
    /// The replies that wait for the output.
    output: Vec<Reply>,
    /// The records whose application the activation handed over.
    applied: Vec<ActivationRecord>,
}

impl Adapter {
    /// Returns the adapter that drives `activation`.
    fn new(activation: Activation<FakeFence>) -> Self {
        Self {
            activation,
            message: Vec::new(),
            output: Vec::new(),
            applied: Vec::new(),
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
                    [ACTIVATE, _, _, record @ ..] => ActivationInput::Activate(record),
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

    /// Does what `action` says, except that it leaves an accepted record's application to the
    /// test.
    fn act(&mut self, action: ActivationAction) {
        match action {
            ActivationAction::Acknowledge => self.output.push(Reply::Acknowledgement),
            ActivationAction::Continue | ActivationAction::Join | ActivationAction::Serve => {}
            ActivationAction::Apply(record) => self.applied.push(*record),
            ActivationAction::Ready(ready) => self.output.push(Reply::Ready(ready)),
        }
    }
}

#[test]
fn the_deadline_stays_live_under_trickled_input_and_blocked_output() {
    let t0 = Instant::now();
    let activate = message(ACTIVATE, &record());
    assert!(activate.len() > 10);
    // A host that trickles its Activate, a byte a second, never completes it by the deadline:
    // the byte at the deadline fails the activation, while the output took nothing.
    let fence = FakeFence::default();
    let mut adapter = Adapter::new(bootstrap(&fence, t0));
    adapter.reset(t0).unwrap();
    let mut failure = None;
    for (second, byte) in (1..).zip(&activate) {
        if let Err(error) = adapter.receive(after(t0, second), &[*byte]) {
            failure = Some((second, error));
            break;
        }
    }
    let (second, failure) = failure.unwrap();
    assert_eq!(second, 10);
    assert_eq!(failure.state(), ActivationState::Restricted);
    assert!(matches!(failure.cause(), ActivationCause::Expired));
    assert!(adapter.applied.is_empty());
    assert_eq!(adapter.output, [Reply::Acknowledgement]);
    assert_final(&mut adapter.activation, &fence, &failure, after(t0, 11), 0);
    // A host that sends nothing leaves the adapter only its wake at the deadline, which fails the
    // activation as well.
    let fence = FakeFence::default();
    let mut adapter = Adapter::new(bootstrap(&fence, t0));
    adapter.reset(after(t0, 1)).unwrap();
    let deadline = adapter.activation.deadline().unwrap();
    assert_eq!(deadline, after(t0, 10));
    adapter
        .activation
        .tick(deadline - Duration::from_millis(1))
        .unwrap();
    let failure = adapter.activation.tick(deadline).unwrap_err();
    assert_eq!(failure.state(), ActivationState::Restricted);
    assert!(matches!(failure.cause(), ActivationCause::Expired));
    assert_final(&mut adapter.activation, &fence, &failure, after(t0, 11), 0);
    // A host whose Activate arrives at once, and that trickles a retry while the record's
    // application runs, gets the retry joined, without another application or a later deadline,
    // and an application that completes after the deadline fails.
    let fence = FakeFence::default();
    let mut adapter = Adapter::new(bootstrap(&fence, t0));
    adapter.reset(t0).unwrap();
    adapter.receive(after(t0, 1), &activate).unwrap();
    assert_eq!(adapter.applied, [accepted()]);
    for (second, byte) in (2..9).zip(&activate) {
        adapter.receive(after(t0, second), &[*byte]).unwrap();
    }
    adapter.receive(after(t0, 9), &activate[7..]).unwrap();
    assert_eq!(adapter.applied, [accepted()]);
    assert_eq!(adapter.activation.deadline(), Some(after(t0, 10)));
    let failure = adapter
        .activation
        .complete(after(t0, 11), Ok(()))
        .unwrap_err();
    assert_eq!(failure.state(), ActivationState::Activating);
    assert!(matches!(failure.cause(), ActivationCause::Expired));
    assert_eq!(adapter.output, [Reply::Acknowledgement]);
    assert_final(&mut adapter.activation, &fence, &failure, after(t0, 12), 0);
    // Another message of the framing, whole, while the record's application runs fails the
    // activation, as it would before the Activate.
    let fence = FakeFence::default();
    let mut adapter = Adapter::new(activating(&fence, t0));
    let failure = adapter
        .receive(after(t0, 1), &message(PING, b""))
        .unwrap_err();
    assert_eq!(failure.state(), ActivationState::Activating);
    assert!(matches!(failure.cause(), ActivationCause::Request));
    assert_final(&mut adapter.activation, &fence, &failure, after(t0, 2), 0);
}

/// A call that fails an activation.
type Failing = Box<dyn Fn(&mut Activation<FakeFence>) -> Result<(), ActivationFailure>>;

#[test]
fn strict_failures_are_returned_and_never_power_off() {
    let t0 = Instant::now();
    let renamed = clone_record(TRANSACTION, b"renamed", resources()).encode();
    let other = clone_record(OTHER_TRANSACTION, b"clone", resources()).encode();
    let cold = ActivationRecord::new(None, configuration(b"clone", resources()))
        .unwrap()
        .encode();
    let more_cpus = ExpectedResources {
        cpus: CPUS + 1,
        memory: MEMORY,
    };
    let unexpected = clone_record(TRANSACTION, b"clone", more_cpus).encode();
    let activate = move |record: Vec<u8>| -> Failing {
        Box::new(move |activation| {
            let input = ActivationInput::Activate(&record);
            activation.input(after(t0, 1), input).map(drop)
        })
    };
    // Each failure, in a state in which it comes: the activation returns it, keeps it, and changes
    // nothing else. It has no system through which it could power the VM off: the adapter reports
    // the failure, and ends the VM.
    let failures: [(Setup, Failing); 11] = [
        (
            restricted,
            Box::new(move |activation| activation.tick(after(t0, 10))),
        ),
        (
            activating,
            Box::new(move |activation| {
                activation
                    .input(after(t0, 1), ActivationInput::Reset)
                    .map(drop)
            }),
        ),
        (
            restricted,
            Box::new(move |activation| {
                activation
                    .input(after(t0, 1), ActivationInput::Request)
                    .map(drop)
            }),
        ),
        (restricted, activate(Vec::new())),
        (restricted, activate(cold)),
        (restricted, activate(unexpected)),
        (ready, activate(other)),
        (started, activate(renamed)),
        (
            activating,
            Box::new(move |activation| {
                activation
                    .complete(after(t0, 1), Err("failed".into()))
                    .map(drop)
            }),
        ),
        (
            bootstrap,
            Box::new(move |activation| activation.complete(after(t0, 1), Ok(())).map(drop)),
        ),
        (activating, Box::new(|activation| activation.start())),
    ];
    let mut causes = Vec::new();
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
        (ActivationState::Bootstrap, "transport bootstrap"),
        (ActivationState::Restricted, "restricted"),
        (ActivationState::Activating, "activating"),
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
            failure(ActivationState::Bootstrap, ActivationCause::Expired),
            "the activation failed in its transport bootstrap state: the deadline passed before \
             the activation was ready",
        ),
        (
            failure(ActivationState::Activating, ActivationCause::Reset),
            "the activation failed in its activating state: the session was reset again before \
             the activation was ready",
        ),
        (
            failure(ActivationState::Restricted, ActivationCause::Request),
            "the activation failed in its restricted state: the host sent a request before the \
             activation was ready",
        ),
        (
            failure(
                ActivationState::Restricted,
                ActivationCause::Record(ActivationError::Truncated),
            ),
            "the activation failed in its restricted state: the Activate's record is refused: \
             the record ends inside its version or one of its fields",
        ),
        (
            failure(ActivationState::Restricted, ActivationCause::Cold),
            "the activation failed in its restricted state: the Activate's record is a cold \
             boot's, without a restore section",
        ),
        (
            failure(
                ActivationState::Restricted,
                ActivationCause::Resources {
                    expected: ExpectedResources {
                        cpus: 4,
                        memory: 2 << 30,
                    },
                    actual: resources(),
                },
            ),
            "the activation failed in its restricted state: the Activate's record expects 4 CPUs \
             and 2147483648 bytes of memory, and the clone has 2 CPUs and 1073741824 bytes",
        ),
        (
            failure(
                ActivationState::Ready,
                ActivationCause::Transaction {
                    accepted: TRANSACTION,
                    offered: OTHER_TRANSACTION,
                },
            ),
            "the activation failed in its ready state: the Activate names the transaction \
             33333333333333333333333333333333, after an Activate of the transaction \
             22222222222222222222222222222222",
        ),
        (
            failure(ActivationState::Started, ActivationCause::Changed),
            "the activation failed in its started state: the Activate changes the record of the \
             accepted Activate's transaction",
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
            failure(ActivationState::Restricted, ActivationCause::Completion),
            "the activation failed in its restricted state: an activation that did not run \
             completed",
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
fn an_activation_debugs_its_state_resources_and_deadline_without_its_fence() {
    let t0 = Instant::now();
    let activation = Activation::with_resources(
        ExpectedResources {
            cpus: 4,
            memory: 2 << 30,
        },
        Some(t0),
        FakeFence::default(),
        TIMEOUT,
        after(t0, 1),
    );
    assert_eq!(
        format!("{activation:?}"),
        format!(
            "Activation {{ state: Bootstrap, resources: ExpectedResources {{ cpus: 4, memory: \
             2147483648 }}, deadline: {:?}, .. }}",
            Some(after(t0, 10))
        )
    );
}
