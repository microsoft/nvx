//! Tests of the restore sequence of clones, [`Restoring::activate`], [`Activated::start`], and
//! [`Started::commit`], with a [`restore_vmm_role`](super::launch) as the clone's VMM, which
//! sends its restore-ready event as its plan says, and a [`FakeControl`] in place of the adapter's
//! framing: the wait for the event, and the connection only once it came; the Activate of the
//! sandbox's record, byte for byte, as the first request of each session; the check of the
//! activation that Ready reports; the start; the sessions that replace those that the adapter lost,
//! after a reset of the session, say, and the clone that ended once it took that reset before it
//! was ready; and the deadlines. The crashes of the sequence's caller are among the launch's
//! tests.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::thread;
use std::time::{Duration, Instant};

use super::launch::{OPENVMM, TIMEOUT, VmmDir, complete_snapshot, fake_vmm};
use super::*;
use crate::activation::{ConfigurationIdentity, ReadyActivation, RestoreSection, TransactionId};
use crate::host::platform::Transport;
use crate::host::process::Process;
use crate::host::store::{CLONE_ACTIVATION_NAME, Recovery};
use crate::host::supervisor::StopOutcome;
use crate::host::template::clone::launch::Hooks;

/// How long a VMM whose plan delays its event waits before it sends it.
const DELAY: Duration = Duration::from_millis(300);

/// A short deadline, which a test expects to pass.
const SHORT: Duration = Duration::from_secs(2);

/// The restore-to-Ready deadline of a clone that never answers its Activate, which leaves its VMM
/// time to start on a busy host.
const SILENT: Duration = Duration::from_secs(10);

/// A call that the restore sequence made to a [`FakeControl`].
#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    /// An attachment of a session, numbered from 1, with the capability, when the call came, by
    /// the deadline.
    Attach {
        session: usize,
        capability: [u8; CAPABILITY_LEN],
        at: Instant,
        deadline: Instant,
    },
    /// An Activate of the record in a session, by the deadline.
    Activate {
        session: usize,
        record: Vec<u8>,
        deadline: Instant,
    },
    /// A start in a session, by the deadline.
    Start { session: usize, deadline: Instant },
}

/// How a [`FakeControl`] answers an Activate or a start.
enum Answer {
    /// With the clone's Ready, which reports the activation of the record that the Activate
    /// carries, or with the start's confirmation.
    Done,
    /// With a Ready that reports this activation, or none.
    Report(Option<ReadyActivation>),
    /// With this failure.
    Fail(Failure),
    /// With no answer until the deadline, then a failure that keeps the session.
    Silent,
}

/// A failure of a [`FakeControl`]: one that lost its session, or another.
#[derive(Debug)]
struct Failure {
    /// Whether the failure lost the session.
    lost: bool,
}

impl Failure {
    /// The loss of the session, as its reset or the end of its stream are.
    const LOST: Self = Self { lost: true };
    /// Another failure, which keeps the session, as the guest's refusal does.
    const REFUSED: Self = Self { lost: false };
}

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(if self.lost {
            "the fake framing lost its session"
        } else {
            "the fake guest refused"
        })
    }
}

impl std::error::Error for Failure {}

/// A session of a [`FakeControl`], over a connection to the clone's control endpoint.
struct FakeSession {
    /// The session's number, from 1.
    number: usize,
    /// The connection, which the session holds while it lasts, as a real one does.
    _transport: Box<dyn Transport>,
}

/// The adapter's framing of the tests: it attaches a session to each connection that the sequence
/// makes, numbered from 1, answers each Activate as the next of its `activates` says, and each
/// start as the next of its `starts` says, as [`Answer::Done`] does once they run out, and records
/// each call. Its `on_activate` runs at each Activate, with the session's number, before the
/// answer.
#[derive(Default)]
struct FakeControl<'a> {
    /// The calls, in order.
    calls: Vec<Call>,
    /// The sessions that it attached.
    sessions: usize,
    /// How it answers the next Activates.
    activates: VecDeque<Answer>,
    /// How it answers the next starts.
    starts: VecDeque<Answer>,
    /// What runs at each Activate.
    on_activate: Option<Box<dyn FnMut(usize) + 'a>>,
}

impl<'a> FakeControl<'a> {
    /// Returns a framing that answers the Activates as `activates` say.
    fn activating(activates: impl IntoIterator<Item = Answer>) -> Self {
        Self {
            activates: activates.into_iter().collect(),
            ..Self::default()
        }
    }

    /// Returns the records of the Activates, in order.
    fn records(&self) -> Vec<&[u8]> {
        self.calls
            .iter()
            .filter_map(|call| match call {
                Call::Activate { record, .. } => Some(record.as_slice()),
                _ => None,
            })
            .collect()
    }

    /// Returns the deadlines of the attachments and Activates, in order.
    fn ready_deadlines(&self) -> Vec<Instant> {
        self.calls
            .iter()
            .filter_map(|call| match call {
                Call::Attach { deadline, .. } | Call::Activate { deadline, .. } => Some(*deadline),
                Call::Start { .. } => None,
            })
            .collect()
    }

    /// Checks that each session of the framing attached, then sent an Activate first, in its
    /// order, and that the starts came last, in the last session.
    fn assert_activates_first(&self) {
        let mut session = 0;
        let mut pending = false;
        for call in &self.calls {
            match call {
                Call::Attach {
                    session: number, ..
                } => {
                    assert!(!pending, "{:?}", self.calls);
                    session += 1;
                    assert_eq!(*number, session, "{:?}", self.calls);
                    pending = true;
                }
                Call::Activate {
                    session: number, ..
                } => {
                    assert!(pending && *number == session, "{:?}", self.calls);
                    pending = false;
                }
                Call::Start {
                    session: number, ..
                } => {
                    assert!(!pending && *number == session, "{:?}", self.calls);
                }
            }
        }
    }
}

/// Answers `answer` by `deadline`, with the Ready `ready` for [`Answer::Done`].
fn answered<T>(answer: Answer, ready: T, deadline: Instant) -> Result<T, Failure>
where
    T: From<Option<ReadyActivation>>,
{
    match answer {
        Answer::Done => Ok(ready),
        Answer::Report(reported) => Ok(T::from(reported)),
        Answer::Fail(failure) => Err(failure),
        Answer::Silent => {
            thread::sleep(deadline.saturating_duration_since(Instant::now()));
            Err(Failure::REFUSED)
        }
    }
}

impl CloneControl for FakeControl<'_> {
    type Session = FakeSession;
    type Error = Failure;

    fn attach(
        &mut self,
        transport: Box<dyn Transport>,
        capability: &[u8; CAPABILITY_LEN],
        deadline: Instant,
    ) -> Result<FakeSession, Failure> {
        self.sessions += 1;
        self.calls.push(Call::Attach {
            session: self.sessions,
            capability: *capability,
            at: Instant::now(),
            deadline,
        });
        Ok(FakeSession {
            number: self.sessions,
            _transport: transport,
        })
    }

    fn activate(
        &mut self,
        session: &mut FakeSession,
        record: &[u8],
        deadline: Instant,
    ) -> Result<Option<ReadyActivation>, Failure> {
        self.calls.push(Call::Activate {
            session: session.number,
            record: record.to_vec(),
            deadline,
        });
        if let Some(on_activate) = &mut self.on_activate {
            on_activate(session.number);
        }
        let ready = ReadyActivation::of(&ActivationRecord::decode(record).unwrap());
        let answer = self.activates.pop_front().unwrap_or(Answer::Done);
        answered(answer, Some(ready), deadline)
    }

    fn start(&mut self, session: &mut FakeSession, deadline: Instant) -> Result<(), Failure> {
        self.calls.push(Call::Start {
            session: session.number,
            deadline,
        });
        let answer = self.starts.pop_front().unwrap_or(Answer::Done);
        answered(answer, Confirmed, deadline).map(|_| ())
    }

    fn lost(&self, error: &Failure) -> bool {
        error.lost
    }
}

/// The confirmation of a start, which [`answered`] returns.
struct Confirmed;

impl From<Option<ReadyActivation>> for Confirmed {
    fn from(_: Option<ReadyActivation>) -> Self {
        Self
    }
}

/// The instants around the launch of a clone, at which its restore-to-Ready deadline started.
struct Launched {
    /// Before the launch.
    before: Instant,
    /// Once the launch returned.
    after: Instant,
}

impl Launched {
    /// Checks that `deadline` is `timeout` after the launch.
    fn assert_deadline(&self, deadline: Instant, timeout: Duration) {
        assert!(
            self.before + timeout <= deadline && deadline <= self.after + timeout,
            "{deadline:?} is not {timeout:?} after the launch"
        );
    }
}

/// Launches the clone of `acquired`, which leases the generation of `fixture`, of `image` in RAM,
/// with a [`restore_vmm_role`](super::launch) in `vmm` as its VMM. Returns the launch, and when it
/// happened.
fn launch<'a>(
    fixture: &Fixture,
    acquired: &'a mut Acquired,
    image: &(PathBuf, DistroBinding),
    vmm: &mut VmmDir,
) -> (Restoring<'a>, Launched) {
    let request = tmpfs_request(&image.0, &image.1);
    let plan = fixture.plan(acquired, &request, NO_CONTENT).unwrap();
    let runtime = plan.key().runtime().clone();
    let dir = vmm.path().to_owned();
    let made = RefCell::new(None);
    let command = fake_vmm(&dir, &made);
    let hooks = Hooks {
        command: &command,
        ..Hooks::LAUNCH
    };
    let before = Instant::now();
    let restoring = plan
        .launch_with(acquired, Path::new(OPENVMM), &runtime, hooks)
        .unwrap();
    let after = Instant::now();
    vmm.reference = Some(restoring.starting().process().reference().clone());
    (restoring, Launched { before, after })
}

/// A fresh fixture whose generation's snapshot is complete, and an image of its clones.
fn fixture() -> (Fixture, (PathBuf, DistroBinding)) {
    let fixture = Fixture::new();
    complete_snapshot(&fixture);
    let image = fixture.image("distro.erofs", IMAGE_LEN);
    (fixture, image)
}

/// Returns a VMM's working directory, whose VMM sends its event as `plan` says.
fn vmm_with(plan: &str) -> VmmDir {
    let vmm = VmmDir::new();
    vmm.plan(plan);
    vmm
}

/// Checks that the VMM of `vmm` has exited.
fn assert_vmm_ended(vmm: &VmmDir) {
    let reference = vmm.reference.as_ref().unwrap();
    if let Some(process) = Process::open(reference).unwrap() {
        assert!(process.wait(Instant::now() + TIMEOUT).unwrap());
    }
}

/// Checks that the restore sequence of the clone of `acquired` ended its launch, after its VMM in
/// `vmm`, and left the lease, which a release then ends.
fn assert_launch_ended(fixture: &Fixture, acquired: &mut Acquired, vmm: &VmmDir) {
    assert_vmm_ended(vmm);
    let locked = &mut acquired.locked;
    assert!(locked.journal().unwrap().is_none());
    assert!(locked.runtime().unwrap().is_none());
    assert_eq!(Lease::of(locked).unwrap(), Some(acquired.lease.clone()));
    assert_eq!(
        fixture.store.release(locked).unwrap(),
        Some(acquired.lease.clone())
    );
}

#[test]
fn a_clone_is_activated_once_its_restore_is_ready_then_started_and_committed() {
    let (fixture, image) = fixture();
    let mut acquired = fixture.acquire("sandbox");
    let record = acquired.locked.clone_activation_path();
    // The VMM sends its event late, and flushes it as OpenVMM does, which waits on Windows until
    // the host read it.
    let mut vmm = vmm_with(&format!("delay:{} flush", DELAY.as_millis()));
    let (restoring, launched) = launch(&fixture, &mut acquired, &image, &mut vmm);
    let activation = restoring.activation().clone();
    let capability = *restoring.capability();
    let endpoint = restoring.starting().endpoint().to_owned();
    let mut control = FakeControl::default();
    let deadlines = RestoreDeadlines::default();
    assert_eq!(deadlines.ready, RESTORE_TO_READY_TIMEOUT);
    assert_eq!(deadlines.start, READY_TO_START_TIMEOUT);
    let activating = Instant::now();
    let mut activated = restoring.activate(&mut control, deadlines).unwrap();
    let ready = Instant::now();
    // The host read the event, and only then connected, and attached one session with the
    // launch's capability, whose first request was the Activate of the sandbox's record, byte for
    // byte the launch's, by the restore-to-Ready deadline, which started with the launch.
    assert!(vmm.started().ends_with(" true"));
    assert!(vmm.flushed());
    control.assert_activates_first();
    let [
        Call::Attach {
            session: 1,
            capability: attached,
            at,
            deadline,
        },
        Call::Activate {
            session: 1,
            record: sent,
            deadline: activated_by,
        },
    ] = control.calls.as_slice()
    else {
        panic!("unexpected calls: {:?}", control.calls);
    };
    assert_eq!(attached, &capability);
    assert!(
        *at >= launched.before + DELAY,
        "the host connected before the event"
    );
    launched.assert_deadline(*deadline, RESTORE_TO_READY_TIMEOUT);
    assert_eq!(activated_by, deadline);
    assert_eq!(sent, &activation.encode());
    assert_eq!(&fs::read(&record).unwrap(), sent);
    // The Ready-to-Start deadline started at the check of the clone's Ready.
    let start = activated.deadline();
    assert!(
        activating + READY_TO_START_TIMEOUT <= start && start <= ready + READY_TO_START_TIMEOUT
    );
    assert_eq!(activated.activation(), &activation);
    assert_eq!(activated.starting().endpoint(), endpoint);
    assert_eq!(activated.session().number, 1);
    let shown = format!("{activated:?}");
    assert!(shown.starts_with("Activated {"), "{shown}");
    assert!(!shown.contains("capability"), "{shown}");
    // The sandbox starts in the same session, by that deadline, and the launch commits.
    let started = activated.start(&mut control).unwrap();
    assert_eq!(
        control.calls.last(),
        Some(&Call::Start {
            session: 1,
            deadline: start
        })
    );
    let (running, session) = started.commit().unwrap();
    assert_eq!(session.number, 1);
    // The runtime record names the VM, whose lease lasts as long as it runs.
    let locked = &mut acquired.locked;
    let recorded = locked.runtime().unwrap().unwrap();
    assert_eq!(recorded.endpoint(), endpoint);
    assert_eq!(running.endpoint(), endpoint);
    assert!(locked.journal().unwrap().is_none());
    let error = fixture.store.release(locked).unwrap_err();
    assert_eq!(error.code(), ErrorCode::SandboxRunning, "{error}");
    assert_eq!(Lease::of(locked).unwrap(), Some(acquired.lease.clone()));
    assert_eq!(
        running.terminate(locked, TIMEOUT).unwrap(),
        StopOutcome::Forced
    );
    assert_eq!(
        fixture.store.release(locked).unwrap(),
        Some(acquired.lease.clone())
    );
}

/// The Ready that a clone reports, from the activation that the record expects, another
/// configuration, and another transaction.
type Reported =
    fn(ReadyActivation, ConfigurationIdentity, TransactionId) -> Option<ReadyActivation>;

#[test]
fn a_ready_that_does_not_report_the_record_s_activation_ends_the_launch() {
    let (fixture, image) = fixture();
    let other_identity = ConfigurationIdentity::from_bytes([0x5a; ConfigurationIdentity::LEN]);
    let other_transaction = TransactionId::from_bytes([0x6b; TransactionId::LEN]);
    let cases: [(&str, Reported); 4] = [
        // Another configuration, in the record's transaction.
        ("identity", |expected, identity, _| {
            Some(ReadyActivation {
                identity,
                ..expected
            })
        }),
        // The record's configuration, in another transaction.
        ("transaction", |expected, _, transaction| {
            Some(ReadyActivation {
                transaction: Some(transaction),
                ..expected
            })
        }),
        // A cold boot's activation of the record's configuration, without a transaction.
        ("cold", |expected, _, _| {
            Some(ReadyActivation {
                transaction: None,
                ..expected
            })
        }),
        // No activation.
        ("none", |_, _, _| None),
    ];
    for (name, reported) in cases {
        let mut acquired = fixture.acquire(name);
        let mut vmm = vmm_with("flush");
        let (restoring, _) = launch(&fixture, &mut acquired, &image, &mut vmm);
        let expected = ReadyActivation::of(restoring.activation());
        let reported = reported(expected, other_identity, other_transaction);
        let mut control = FakeControl::activating([Answer::Report(reported)]);
        let error = restoring
            .activate(&mut control, RestoreDeadlines::default())
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Protocol, "{name}: {error}");
        assert!(
            error.message().starts_with("the clone's Ready reports"),
            "{error}"
        );
        // One session, which the host ended, and no start.
        assert_eq!(control.calls.len(), 2, "{name}: {:?}", control.calls);
        assert_launch_ended(&fixture, &mut acquired, &vmm);
    }
}

#[test]
fn a_session_that_the_adapter_lost_is_replaced_by_one_that_resends_the_same_record_first() {
    let (fixture, image) = fixture();
    let mut acquired = fixture.acquire("sandbox");
    let record = acquired.locked.clone_activation_path();
    let mut vmm = vmm_with("flush");
    let (restoring, launched) = launch(&fixture, &mut acquired, &image, &mut vmm);
    // The adapter loses its first session as it awaits the clone's Ready, as a reset of the
    // session does: the clone, which was ready, answers the Activate of a new session.
    let mut control = FakeControl::activating([Answer::Fail(Failure::LOST), Answer::Done]);
    let activated = restoring
        .activate(&mut control, RestoreDeadlines::default())
        .unwrap();
    control.assert_activates_first();
    assert_eq!(control.sessions, 2);
    // Both Activates carry the sandbox's record, byte for byte, by the same deadline, which the
    // retry did not extend.
    let recorded = fs::read(&record).unwrap();
    assert_eq!(control.records(), [&recorded[..], &recorded[..]]);
    let deadlines = control.ready_deadlines();
    assert!(deadlines.iter().all(|deadline| *deadline == deadlines[0]));
    launched.assert_deadline(deadlines[0], RESTORE_TO_READY_TIMEOUT);
    // The sandbox starts in the new session.
    let started = activated.start(&mut control).unwrap();
    assert!(matches!(
        control.calls.last(),
        Some(Call::Start { session: 2, .. })
    ));
    assert_eq!(
        started.abandon().unwrap(),
        Recovery::Terminated(vmm.reference.clone().unwrap())
    );
    assert_launch_ended(&fixture, &mut acquired, &vmm);
}

#[test]
fn a_retry_never_sends_another_record_nor_takes_another_activation() {
    let (fixture, image) = fixture();

    // The sandbox's record changed once the first session was lost: the retry sends nothing, and
    // connects no more.
    let mut acquired = fixture.acquire("changed");
    let record = acquired.locked.clone_activation_path();
    let mut vmm = vmm_with("flush");
    let (restoring, _) = launch(&fixture, &mut acquired, &image, &mut vmm);
    let original = restoring.activation().clone();
    let restore = original.restore().unwrap();
    let other = ActivationRecord::new(
        Some(RestoreSection {
            transaction: TransactionId::from_bytes([0x77; TransactionId::LEN]),
            template: restore.template,
        }),
        original.configuration().clone(),
    )
    .unwrap();
    let path = record.clone();
    let mut control = FakeControl::activating([Answer::Fail(Failure::LOST)]);
    control.on_activate = Some(Box::new(move |_| {
        fs::write(&path, other.encode()).unwrap();
    }));
    let error = restoring
        .activate(&mut control, RestoreDeadlines::default())
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("no longer holds"), "{error}");
    assert_eq!(control.sessions, 1);
    assert_eq!(control.records(), [&original.encode()[..]]);
    assert_launch_ended(&fixture, &mut acquired, &vmm);

    // The Ready of the retry reports another transaction.
    let mut acquired = fixture.acquire("diverged");
    let mut vmm = vmm_with("flush");
    let (restoring, _) = launch(&fixture, &mut acquired, &image, &mut vmm);
    let expected = ReadyActivation::of(restoring.activation());
    let diverged = ReadyActivation {
        transaction: Some(TransactionId::from_bytes([0x78; TransactionId::LEN])),
        ..expected
    };
    let mut control =
        FakeControl::activating([Answer::Fail(Failure::LOST), Answer::Report(Some(diverged))]);
    let error = restoring
        .activate(&mut control, RestoreDeadlines::default())
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Protocol, "{error}");
    assert_eq!(control.sessions, 2);
    assert_launch_ended(&fixture, &mut acquired, &vmm);
}

#[test]
fn only_lost_sessions_are_replaced_and_only_so_many_times() {
    let (fixture, image) = fixture();

    // A failure that did not lose the session is final.
    let mut acquired = fixture.acquire("refused");
    let mut vmm = vmm_with("flush");
    let (restoring, _) = launch(&fixture, &mut acquired, &image, &mut vmm);
    let mut control = FakeControl::activating([Answer::Fail(Failure::REFUSED)]);
    let error = restoring
        .activate(&mut control, RestoreDeadlines::default())
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Protocol, "{error}");
    assert_eq!(error.message(), "cannot activate it");
    assert_eq!(control.sessions, 1);
    assert_launch_ended(&fixture, &mut acquired, &vmm);

    // A session lost again and again ends the sequence once the last of its sessions is lost.
    let mut acquired = fixture.acquire("lost");
    let mut vmm = vmm_with("flush");
    let (restoring, _) = launch(&fixture, &mut acquired, &image, &mut vmm);
    let lost = (0..MAX_ACTIVATION_SESSIONS).map(|_| Answer::Fail(Failure::LOST));
    let mut control = FakeControl::activating(lost);
    let error = restoring
        .activate(&mut control, RestoreDeadlines::default())
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Protocol, "{error}");
    assert!(error.message().contains("was lost"), "{error}");
    control.assert_activates_first();
    assert_eq!(control.sessions, MAX_ACTIVATION_SESSIONS);
    assert_launch_ended(&fixture, &mut acquired, &vmm);
}

#[test]
fn a_clone_that_took_the_reset_of_a_lost_session_before_its_ready_ends_the_retries() {
    let (fixture, image) = fixture();
    let mut acquired = fixture.acquire("sandbox");
    let mut vmm = vmm_with("flush");
    let (restoring, _) = launch(&fixture, &mut acquired, &image, &mut vmm);
    // The reset of the lost session ends a clone that was not ready yet, and with it its VMM: the
    // retry cannot connect any more.
    let ending = &vmm;
    let mut control = FakeControl::activating([Answer::Fail(Failure::LOST)]);
    control.on_activate = Some(Box::new(move |_| ending.end_and_wait()));
    let error = restoring
        .activate(&mut control, RestoreDeadlines::default())
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Exited, "{error}");
    assert_eq!(control.sessions, 1);
    drop(control);
    assert_launch_ended(&fixture, &mut acquired, &vmm);
}

#[test]
fn the_restore_to_ready_deadline_runs_from_the_launch_through_every_step() {
    let (fixture, image) = fixture();
    let deadlines = RestoreDeadlines {
        ready: SHORT,
        ..RestoreDeadlines::default()
    };

    // A VMM that sends no event: the wait ends at the deadline, before any connection.
    let mut acquired = fixture.acquire("no-event");
    let mut vmm = vmm_with("none");
    let (restoring, launched) = launch(&fixture, &mut acquired, &image, &mut vmm);
    let mut control = FakeControl::default();
    let error = restoring.activate(&mut control, deadlines).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TimedOut, "{error}");
    assert!(launched.before.elapsed() >= SHORT);
    assert!(control.calls.is_empty());
    assert_launch_ended(&fixture, &mut acquired, &vmm);

    // A clone that never answers its Activate, after a late event: the attachment and the
    // Activate have the deadline that started with the launch.
    let mut acquired = fixture.acquire("no-ready");
    let mut vmm = vmm_with(&format!("delay:{} flush", DELAY.as_millis()));
    let (restoring, launched) = launch(&fixture, &mut acquired, &image, &mut vmm);
    let mut control = FakeControl::activating([Answer::Silent]);
    let deadlines = RestoreDeadlines {
        ready: SILENT,
        ..RestoreDeadlines::default()
    };
    let error = restoring.activate(&mut control, deadlines).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TimedOut, "{error}");
    assert_eq!(control.sessions, 1);
    for deadline in control.ready_deadlines() {
        launched.assert_deadline(deadline, deadlines.ready);
    }
    assert!(launched.before.elapsed() >= deadlines.ready);
    assert_launch_ended(&fixture, &mut acquired, &vmm);
}

#[test]
fn the_ready_to_start_deadline_runs_from_the_ready_and_a_start_is_never_resent() {
    let (fixture, image) = fixture();
    let deadlines = RestoreDeadlines {
        start: SHORT,
        ..RestoreDeadlines::default()
    };

    // A start that the guest never confirms ends at the deadline that started at the Ready.
    let mut acquired = fixture.acquire("silent");
    let mut vmm = vmm_with("flush");
    let (restoring, _) = launch(&fixture, &mut acquired, &image, &mut vmm);
    let mut control = FakeControl::default();
    control.starts.push_back(Answer::Silent);
    let ready = Instant::now();
    let activated = restoring.activate(&mut control, deadlines).unwrap();
    let start = activated.deadline();
    assert!(start >= ready + SHORT);
    let error = activated.start(&mut control).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TimedOut, "{error}");
    assert!(Instant::now() >= start);
    assert_eq!(
        control.calls.last(),
        Some(&Call::Start {
            session: 1,
            deadline: start
        })
    );
    assert_launch_ended(&fixture, &mut acquired, &vmm);

    // A start that fails, even in a lost session, is not sent again.
    let mut acquired = fixture.acquire("lost");
    let mut vmm = vmm_with("flush");
    let (restoring, _) = launch(&fixture, &mut acquired, &image, &mut vmm);
    let mut control = FakeControl::default();
    control.starts.push_back(Answer::Fail(Failure::LOST));
    let activated = restoring.activate(&mut control, deadlines).unwrap();
    let error = activated.start(&mut control).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Protocol, "{error}");
    let starts = control
        .calls
        .iter()
        .filter(|call| matches!(call, Call::Start { .. }))
        .count();
    assert_eq!((control.sessions, starts), (1, 1));
    assert_launch_ended(&fixture, &mut acquired, &vmm);
}

#[test]
fn a_vmm_that_exits_or_sends_anything_but_its_event_ends_the_launch() {
    let (fixture, image) = fixture();
    // A VMM that exits before its event, as OpenVMM does once it cannot restore the snapshot, ends
    // the wait at once; another version of the event, and part of it, are refused.
    let cases = [
        ("exit", ErrorKind::Exited),
        ("other flush", ErrorKind::Protocol),
        ("partial flush", ErrorKind::Protocol),
    ];
    for (plan, kind) in cases {
        let mut acquired = fixture.acquire(&plan.replace(' ', "-"));
        let mut vmm = vmm_with(plan);
        let (restoring, launched) = launch(&fixture, &mut acquired, &image, &mut vmm);
        let mut control = FakeControl::default();
        let error = restoring
            .activate(&mut control, RestoreDeadlines::default())
            .unwrap_err();
        assert_eq!(error.kind(), kind, "{plan}: {error}");
        assert!(launched.before.elapsed() < TIMEOUT, "{plan}");
        assert!(control.calls.is_empty(), "{plan}");
        assert_launch_ended(&fixture, &mut acquired, &vmm);
    }

    // Another process than the VMM connects to the restore-ready endpoint.
    let mut acquired = fixture.acquire("intruder");
    let mut vmm = vmm_with("none");
    let (restoring, _) = launch(&fixture, &mut acquired, &image, &mut vmm);
    let ready = PathBuf::from(restoring.ready_endpoint());
    #[cfg(unix)]
    let _intruder = std::os::unix::net::UnixStream::connect(&ready).unwrap();
    #[cfg(windows)]
    let _intruder = fs::OpenOptions::new().write(true).open(&ready).unwrap();
    let mut control = FakeControl::default();
    let error = restoring
        .activate(&mut control, RestoreDeadlines::default())
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Protocol, "{error}");
    let source = std::error::Error::source(&error).unwrap().to_string();
    assert!(source.contains("not the VMM"), "{source}");
    assert!(control.calls.is_empty());
    assert_launch_ended(&fixture, &mut acquired, &vmm);
    assert!(!acquired.locked.dir().join(CLONE_ACTIVATION_NAME).exists());
}
