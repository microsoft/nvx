//! Tests of the restore sequence of clones, [`Restoring::activate`], [`Activated::start`], and
//! [`Started::commit`], with a [`restore_vmm_role`](super::launch) as the clone's VMM, which
//! sends its restore-ready event as its plan says, and a [`FakeControl`] in place of the adapter's
//! framing: the wait for the event, and the connection only once it came; the Activate of the
//! sandbox's record, byte for byte, as the first request of each session; the check of the
//! activation that Ready reports; the mapping table of a clone with an export, and the end of the
//! launch at a partial or failed table; the start; the sessions that replace those that the
//! adapter lost, after a reset of the session, say, and the clone that ended once it took that
//! reset before it was ready; and the deadlines. The crashes of the sequence's caller are among
//! the launch's tests.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::thread;
use std::time::{Duration, Instant};

use super::launch::{
    OPENVMM, TIMEOUT, VmmDir, assert_recorded, complete_snapshot, failed, fake_vmm, runtime_of,
};
use super::*;
use crate::activation::{ReadyActivation, RestoreSection, TransactionId};
use crate::host::platform::Transport;
use crate::host::process::{BootId, Process};
use crate::host::store::{CLONE_ACTIVATION_NAME, Recovery};
use crate::host::supervisor::{Starting, StopOutcome};
use crate::host::template::clone::launch::Hooks;
use crate::host::template::clone::records::{End, read_failure};
use crate::protocol::{
    APP_HEADER_LEN, OUTER_MAX_PAYLOAD, decode_maps_payload, encode_maps_payloads,
};

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
    /// A mapping table in a session, by the deadline, as the guest decodes the `MAPS` payloads in
    /// which the framing encodes it.
    Map {
        session: usize,
        table: Vec<HostMap>,
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

/// A failure of a [`FakeControl`]: one that lost its session, the guest's refusal of a mapping
/// table, or another.
#[derive(Debug)]
struct Failure {
    /// Whether the failure lost the session.
    lost: bool,
    /// Whether the failure is the guest's refusal of a mapping table, which the framing tells.
    table: bool,
}

impl Failure {
    /// The loss of the session, as its reset or the end of its stream are.
    const LOST: Self = Self {
        lost: true,
        table: false,
    };
    /// Another failure, which keeps the session, as the guest's refusal does.
    const REFUSED: Self = Self {
        lost: false,
        table: false,
    };
    /// The guest's refusal of a mapping table, which keeps the session, and which the framing
    /// tells, as [`CloneControl::refused`] does.
    const TABLE: Self = Self {
        lost: false,
        table: true,
    };
}

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(if self.lost {
            "the fake framing lost its session"
        } else if self.table {
            "the fake guest refused its mapping table"
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
/// makes, numbered from 1, answers each Activate as the next of its `activates` says, each mapping
/// table, which it encodes in `MAPS` payloads that its guest decodes, as the next of its `maps`
/// says, and each start as the next of its `starts` says, as [`Answer::Done`] does once they run
/// out, and records each call. Its `on_activate` runs at each Activate, with the session's number,
/// before the answer.
#[derive(Default)]
struct FakeControl<'a> {
    /// The calls, in order.
    calls: Vec<Call>,
    /// The sessions that it attached.
    sessions: usize,
    /// How it answers the next Activates.
    activates: VecDeque<Answer>,
    /// How its guest answers the next mapping tables: how many of a table's mappings it applies,
    /// in order, at most all, and its answer.
    maps: VecDeque<(usize, Answer)>,
    /// The mappings that its guest applied, in order.
    applied: Vec<HostMap>,
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
                Call::Map { .. } | Call::Start { .. } => None,
            })
            .collect()
    }

    /// Checks that each session of the framing attached, then sent an Activate first, in its
    /// order, and that the mapping tables and the starts came last, in the last session.
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
                Call::Map {
                    session: number, ..
                }
                | Call::Start {
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

    fn map(
        &mut self,
        session: &mut FakeSession,
        table: &[HostMap],
        deadline: Instant,
    ) -> Result<(), Failure> {
        // The framing sends the table in `MAPS` payloads, which the guest decodes.
        let parts = encode_maps_payloads(table, OUTER_MAX_PAYLOAD - APP_HEADER_LEN)
            .expect("the framing encodes the clone's table");
        let decoded: Vec<HostMap> = parts
            .iter()
            .flat_map(|part| decode_maps_payload(part).unwrap().maps)
            .collect();
        let (applied, answer) = self
            .maps
            .pop_front()
            .unwrap_or((decoded.len(), Answer::Done));
        self.applied
            .extend_from_slice(&decoded[..applied.min(decoded.len())]);
        self.calls.push(Call::Map {
            session: session.number,
            table: decoded,
            deadline,
        });
        answered(answer, Confirmed, deadline).map(|_| ())
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

    fn refused(&self, error: &Failure) -> bool {
        error.table
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
    launch_ending(fixture, acquired, image, vmm, Hooks::LAUNCH.end)
}

/// Launches the clone as [`launch`] does, whose failures end the launch with `end`.
fn launch_ending<'a>(
    fixture: &Fixture,
    acquired: &'a mut Acquired,
    image: &(PathBuf, DistroBinding),
    vmm: &mut VmmDir,
    end: End,
) -> (Restoring<'a>, Launched) {
    let request = tmpfs_request(&image.0, &image.1);
    launch_request(fixture, acquired, &request, vmm, end)
}

/// Launches the clone of `acquired` as [`launch`] does, as `request` asks, whose failures end the
/// launch with `end`.
fn launch_request<'a>(
    fixture: &Fixture,
    acquired: &'a mut Acquired,
    request: &CloneRequest,
    vmm: &mut VmmDir,
    end: End,
) -> (Restoring<'a>, Launched) {
    let plan = fixture.plan(acquired, request, NO_CONTENT).unwrap();
    let runtime = plan.key().runtime().clone();
    let dir = vmm.path().to_owned();
    let made = RefCell::new(None);
    let command = fake_vmm(&dir, &made);
    let hooks = Hooks {
        command: &command,
        end,
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

/// Checks that `failure`, of the clone of `acquired`, failed at `stage`, once the clone got as far
/// as `progress`, with `evidence`, and the teardown `teardown`, as [`assert_recorded`] does.
fn assert_failed(
    failure: &CloneFailure,
    acquired: &Acquired,
    at: (Stage, Progress),
    evidence: Evidence,
    teardown: Teardown,
) {
    assert_recorded(
        failure,
        &acquired.lease,
        &acquired.locked,
        at,
        evidence,
        teardown,
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
    // The progress log records each step, the spawn and the acknowledgment with the steps that
    // follow them at once, after a first line that names the launch and the current boot of the
    // host, and the sandbox's records show the committed clone.
    let log = fs::read_to_string(acquired.locked.clone_progress_path()).unwrap();
    assert_eq!(
        log,
        format!(
            "nvx-clone-progress 1 {} {}\nspawned\nevent\nacknowledged\nactivating\nready\n\
             starting\nstarted\ncommitted\n",
            acquired.lease.launch(),
            BootId::current().unwrap()
        )
    );
    let records = CloneRecords::read(&acquired.locked).unwrap().unwrap();
    assert_eq!(
        (records.progress, records.failure, records.journal),
        (Progress::Committed, None, None)
    );
    assert_eq!(records.lease, acquired.lease);
    assert_eq!(records.runtime, acquired.locked.runtime().unwrap());
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

/// The Ready that a clone reports, given another transaction than the record's.
type Reported = fn(TransactionId) -> Option<ReadyActivation>;

#[test]
fn a_ready_that_does_not_report_the_record_s_activation_ends_the_launch() {
    let (fixture, image) = fixture();
    let other_transaction = TransactionId::from_bytes([0x6b; TransactionId::LEN]);
    let cases: [(&str, Reported); 3] = [
        // Another transaction.
        ("transaction", |transaction| {
            Some(ReadyActivation {
                transaction: Some(transaction),
            })
        }),
        // A cold boot's activation, without a transaction.
        ("cold", |_| Some(ReadyActivation { transaction: None })),
        // No activation.
        ("none", |_| None),
    ];
    for (name, reported) in cases {
        let mut acquired = fixture.acquire(name);
        let mut vmm = vmm_with("flush");
        let (restoring, _) = launch(&fixture, &mut acquired, &image, &mut vmm);
        let reported = reported(other_transaction);
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
        let ready = (Stage::Activation, Progress::Activating);
        let terminated = Teardown::Terminated;
        assert_failed(&error, &acquired, ready, Evidence::Protocol, terminated);
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
    // The host's own record changed: the record of the failure names the launch's transaction.
    let record = error.record();
    assert_eq!(
        (record.evidence, record.outcome, record.stage),
        (Evidence::HostFailure, Outcome::Host, Stage::Activation)
    );
    assert_eq!(record.transaction, Some(restore.transaction));
    assert_launch_ended(&fixture, &mut acquired, &vmm);

    // The Ready of the retry reports another transaction.
    let mut acquired = fixture.acquire("diverged");
    let mut vmm = vmm_with("flush");
    let (restoring, _) = launch(&fixture, &mut acquired, &image, &mut vmm);
    let diverged = ReadyActivation {
        transaction: Some(TransactionId::from_bytes([0x78; TransactionId::LEN])),
    };
    let mut control =
        FakeControl::activating([Answer::Fail(Failure::LOST), Answer::Report(Some(diverged))]);
    let error = restoring
        .activate(&mut control, RestoreDeadlines::default())
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Protocol, "{error}");
    assert_eq!(control.sessions, 2);
    let ready = (Stage::Activation, Progress::Activating);
    assert_failed(
        &error,
        &acquired,
        ready,
        Evidence::Protocol,
        Teardown::Terminated,
    );
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
    let ready = (Stage::Activation, Progress::Activating);
    assert_failed(
        &error,
        &acquired,
        ready,
        Evidence::Protocol,
        Teardown::Terminated,
    );
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
    assert_failed(
        &error,
        &acquired,
        ready,
        Evidence::Reset,
        Teardown::Terminated,
    );
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
    // The clone's status was lost with its VMM, which ended without a report.
    let ready = (Stage::Activation, Progress::Activating);
    assert_failed(
        &error,
        &acquired,
        ready,
        Evidence::LostStatus,
        Teardown::Exited,
    );
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
    let restore = (Stage::Restore, Progress::Spawned);
    assert_failed(
        &error,
        &acquired,
        restore,
        Evidence::Timeout,
        Teardown::Terminated,
    );
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
    let ready = (Stage::Activation, Progress::Activating);
    assert_failed(
        &error,
        &acquired,
        ready,
        Evidence::Timeout,
        Teardown::Terminated,
    );
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
    let starting = (Stage::Start, Progress::Starting);
    assert_failed(
        &error,
        &acquired,
        starting,
        Evidence::Timeout,
        Teardown::Terminated,
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
    assert_failed(
        &error,
        &acquired,
        starting,
        Evidence::Reset,
        Teardown::Terminated,
    );
    assert_launch_ended(&fixture, &mut acquired, &vmm);
}

#[test]
fn a_vmm_that_exits_or_sends_anything_but_its_event_ends_the_launch() {
    let (fixture, image) = fixture();
    // A VMM that exits before its event, as OpenVMM does once it cannot restore the snapshot, ends
    // the wait at once, and loses the clone's status, unless OpenVMM's fatal error tells that the
    // host cannot present the template's CPU or time, or bind a forward of the clone's network;
    // another version of the event, and part of it, are refused, and the VMM that sent them is
    // terminated.
    let cases = [
        (
            "exit",
            "exit",
            ErrorKind::Exited,
            Evidence::LostStatus,
            Teardown::Exited,
        ),
        (
            "incompatible",
            "exit fatal:E_TSC_RATE_TOLERANCE",
            ErrorKind::Exited,
            Evidence::IncompatibleHost,
            Teardown::Exited,
        ),
        (
            "contract",
            "exit fatal:E_MANIFEST_TIME",
            ErrorKind::Exited,
            Evidence::LostStatus,
            Teardown::Exited,
        ),
        // OpenVMM connects the event's endpoint before it launches its VM worker, whose launch
        // fails on a host that cannot present the template's CPU or time: it closes the endpoint
        // without the event, and reports its error only as it exits, which the wait awaits.
        (
            "closed",
            "close fatal:E_TSC_RATE_TOLERANCE",
            ErrorKind::Exited,
            Evidence::IncompatibleHost,
            Teardown::Exited,
        ),
        // The launch of its VM worker fails too where OpenVMM cannot bind a forward of the
        // clone's network as it restores the clone's NIC, because another socket holds its port.
        (
            "forward",
            "close forward:18080",
            ErrorKind::Exited,
            Evidence::ForwardBind,
            Teardown::Exited,
        ),
        (
            "other",
            "other flush",
            ErrorKind::Protocol,
            Evidence::Protocol,
            Teardown::Terminated,
        ),
        (
            "partial",
            "partial flush",
            ErrorKind::Protocol,
            Evidence::Protocol,
            Teardown::Terminated,
        ),
    ];
    let restore = (Stage::Restore, Progress::Spawned);
    for (name, plan, kind, evidence, teardown) in cases {
        let mut acquired = fixture.acquire(name);
        let mut vmm = vmm_with(plan);
        let (restoring, launched) = launch(&fixture, &mut acquired, &image, &mut vmm);
        let mut control = FakeControl::default();
        let error = restoring
            .activate(&mut control, RestoreDeadlines::default())
            .unwrap_err();
        assert_eq!(error.kind(), kind, "{plan}: {error}");
        assert!(launched.before.elapsed() < TIMEOUT, "{plan}");
        assert!(control.calls.is_empty(), "{plan}");
        assert_failed(&error, &acquired, restore, evidence, teardown);
        // The record keeps the line of OpenVMM's fatal error, and not the warning before it, with
        // the cause of the error's chain that names the forward that OpenVMM cannot bind.
        let line = plan.split_whitespace().find_map(|word| {
            if let Some(code) = word.strip_prefix("fatal:") {
                Some(format!("[{code}] failed to launch vm worker"))
            } else {
                word.strip_prefix("forward:").map(|port| {
                    format!(
                        "failed to launch vm worker: failed to create socket for port forward \
                         (Tcp 127.0.0.1:{port})"
                    )
                })
            }
        });
        if let Some(line) = line {
            let line = format!("OpenVMM reported: fatal error: {line}");
            assert!(error.message().ends_with(&line), "{error}");
            assert!(error.record().message.ends_with(&line), "{error}");
            assert_eq!(error.outcome(), evidence.outcome(), "{error}");
        }
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
    assert_failed(
        &error,
        &acquired,
        restore,
        Evidence::Protocol,
        Teardown::Terminated,
    );
    assert_launch_ended(&fixture, &mut acquired, &vmm);
    assert!(!acquired.locked.dir().join(CLONE_ACTIVATION_NAME).exists());
}

#[test]
fn a_status_with_which_the_guest_ended_the_clone_s_vm_attributes_its_failure() {
    let (fixture, image) = fixture();
    let ready = (Stage::Activation, Progress::Activating);
    let starting = (Stage::Start, Progress::Starting);
    // Each VMM ends as its clone awaits its Ready, or once the start of its sandbox was sent, with
    // OpenVMM's outcome report, which the failure's record reads once the VMM has exited.
    let cases = [
        // The guest's activation could not assemble the clone's root, which only the request's
        // image and upper make, before any workload ran.
        (
            "root",
            "guest-exit:54",
            ready,
            Evidence::UnusableRoot,
            Some(54),
        ),
        // A repair's status is evidence of the repair stage at any point.
        (
            "repair",
            "guest-exit:195",
            ready,
            Evidence::RepairStatus,
            Some(195),
        ),
        (
            "late-repair",
            "guest-exit:195",
            starting,
            Evidence::RepairStatus,
            Some(195),
        ),
        // Once the start was sent, a workload may have ended its VM with any status, so the loss
        // of the session is all that the host knows.
        (
            "late-root",
            "guest-exit:54",
            starting,
            Evidence::Reset,
            Some(54),
        ),
        // A failure of the VMM tells nothing of the guest.
        ("vmm", "vmm-failure:1", ready, Evidence::LostStatus, None),
    ];
    for (name, report, at, evidence, status) in cases {
        let mut acquired = fixture.acquire(name);
        let mut vmm = vmm_with(&format!("flush report:{report}"));
        let (restoring, _) = launch(&fixture, &mut acquired, &image, &mut vmm);
        let error = if at == starting {
            let mut control = FakeControl::default();
            control.starts.push_back(Answer::Fail(Failure::LOST));
            let activated = restoring
                .activate(&mut control, RestoreDeadlines::default())
                .unwrap();
            vmm.end_and_wait();
            activated.start(&mut control).unwrap_err()
        } else {
            let ending = &vmm;
            let mut control = FakeControl::activating([Answer::Fail(Failure::LOST)]);
            control.on_activate = Some(Box::new(move |_| ending.end_and_wait()));
            restoring
                .activate(&mut control, RestoreDeadlines::default())
                .unwrap_err()
        };
        assert_failed(&error, &acquired, at, evidence, Teardown::Exited);
        assert_eq!(error.record().status, status, "{name}");
        assert_launch_ended(&fixture, &mut acquired, &vmm);
    }
}

thread_local! {
    /// The path of the record of a failure that [`ended_once_recorded`] reads, and the record that
    /// it read there.
    static RECORDED: RefCell<Option<(PathBuf, Option<FailureRecord>)>> = const { RefCell::new(None) };
}

/// Ends a launch as [`Starting::abandon`] does, once it read the record of the failure at the path
/// in [`RECORDED`], as the teardown begins, which a crash of the caller during the teardown would
/// leave.
fn ended_once_recorded(starting: Starting<'_>) -> Result<Recovery> {
    RECORDED.with_borrow_mut(|recorded| {
        let (path, record) = recorded.as_mut().expect("the test names the record's path");
        *record = read_failure(path).unwrap();
    });
    starting.abandon()
}

#[test]
fn a_failure_is_recorded_before_its_teardown_and_again_once_the_teardown_ended() {
    let (fixture, image) = fixture();
    let mut acquired = fixture.acquire("sandbox");
    RECORDED.set(Some((acquired.locked.clone_failure_path(), None)));
    let mut vmm = vmm_with("other flush");
    let (restoring, _) = launch_ending(
        &fixture,
        &mut acquired,
        &image,
        &mut vmm,
        ended_once_recorded,
    );
    let mut control = FakeControl::default();
    let error = restoring
        .activate(&mut control, RestoreDeadlines::default())
        .unwrap_err();
    // As the teardown began, the sandbox recorded the failure, with its teardown pending.
    let (_, pending) = RECORDED.take().unwrap();
    let pending = pending.expect("the failure was recorded before its teardown");
    let restore = (Stage::Restore, Progress::Spawned);
    assert_eq!(
        (pending.stage, pending.progress, pending.evidence),
        (restore.0, restore.1, Evidence::Protocol)
    );
    assert_eq!(pending.teardown, Teardown::Pending);
    assert_eq!(pending.message, error.record().message);
    // Once the teardown ended, the record holds its outcome.
    assert_failed(
        &error,
        &acquired,
        restore,
        Evidence::Protocol,
        Teardown::Terminated,
    );
    assert_launch_ended(&fixture, &mut acquired, &vmm);
}

/// Why [`kept`] keeps a launch.
const KEPT: &str = "the test keeps the launch";

/// Ends no launch of a clone that failed: keeps its journal and its VMM, as a launch whose VMM's
/// identity or exit is indeterminate keeps them.
fn kept(starting: Starting<'_>) -> Result<Recovery> {
    // Nothing of the launch ends: its journal and its VMM outlive it, as a crash leaves them.
    std::mem::forget(starting);
    Ok(Recovery::Indeterminate(KEPT.to_owned()))
}

#[test]
fn a_launch_that_cannot_be_ended_keeps_its_records_and_its_lease_until_a_reattach_ends_it() {
    let (fixture, image) = fixture();
    let mut acquired = fixture.acquire("sandbox");
    let mut vmm = vmm_with("flush");
    let (restoring, _) = launch_ending(&fixture, &mut acquired, &image, &mut vmm, kept);
    let mut control = FakeControl::activating([Answer::Fail(Failure::REFUSED)]);
    let error = restoring
        .activate(&mut control, RestoreDeadlines::default())
        .unwrap_err();
    let ready = (Stage::Activation, Progress::Activating);
    let unresolved = Teardown::Unresolved(KEPT.to_owned());
    assert_failed(&error, &acquired, ready, Evidence::Protocol, unresolved);
    let unended = format!("; the launch could not be ended, and remains for recovery: {KEPT}");
    assert!(error.message().ends_with(&unended), "{error}");
    // The journal remains, and with it the lease, which no release ends, and the clone's records.
    let reference = vmm.reference.clone().unwrap();
    let locked = &mut acquired.locked;
    let refusal = fixture.store.release(locked).unwrap_err();
    assert_eq!(refusal.code(), ErrorCode::SandboxRunning, "{refusal}");
    let records = CloneRecords::read(locked).unwrap().unwrap();
    assert_eq!(records.journal.unwrap().process(), Some(&reference));
    assert!(Process::open(&reference).unwrap().is_some());
    // A reattach ends the launch through its VMM's verified identity: the record keeps the
    // failure's stage, evidence, and original outcome, with the teardown that resolved it.
    let mut control = FakeControl::default();
    let reattached = reattach_clone(
        locked,
        &runtime_of(&fixture),
        &mut control,
        Instant::now() + TIMEOUT,
        TIMEOUT,
    )
    .unwrap();
    let resolved = failed(reattached);
    assert_failed(
        &resolved,
        &acquired,
        ready,
        Evidence::Protocol,
        Teardown::Terminated,
    );
    assert_eq!(
        (resolved.code(), resolved.message()),
        (error.code(), error.record().message.as_str())
    );
    assert!(control.calls.is_empty());
    assert_launch_ended(&fixture, &mut acquired, &vmm);
}

#[test]
fn a_caller_fails_a_ready_or_started_clone_with_its_own_evidence_as_the_sequence_fails_one() {
    let (fixture, image) = fixture();
    let ready = (Stage::Start, Progress::Ready);
    // A step of the caller's own, such as one that maps a directory of the host into the clone,
    // fails once the clone is ready, which the request's mappings attribute to the request.
    let mut acquired = fixture.acquire("ready");
    let mut vmm = vmm_with("flush");
    let (restoring, _) = launch(&fixture, &mut acquired, &image, &mut vmm);
    let mut control = FakeControl::default();
    let activated = restoring
        .activate(&mut control, RestoreDeadlines::default())
        .unwrap();
    let mapping = "the test's mapping names no directory of the host";
    let failure = activated.fail(Evidence::InvalidMappings, Error::unsupported(mapping));
    assert_eq!(failure.kind(), ErrorKind::Unsupported, "{failure}");
    assert_eq!(failure.message(), mapping);
    let mappings = Evidence::InvalidMappings;
    assert_failed(&failure, &acquired, ready, mappings, Teardown::Terminated);
    assert_eq!(failure.outcome(), Outcome::Request);
    assert_launch_ended(&fixture, &mut acquired, &vmm);
    // A check of the caller's own fails once the clone's sandbox started, before the commit.
    let mut acquired = fixture.acquire("started");
    let mut vmm = vmm_with("flush");
    let (restoring, _) = launch(&fixture, &mut acquired, &image, &mut vmm);
    let mut control = FakeControl::default();
    let activated = restoring
        .activate(&mut control, RestoreDeadlines::default())
        .unwrap();
    let started = activated.start(&mut control).unwrap();
    let failure = started.fail(Evidence::HostFailure, Error::io("the test's check failed"));
    let commit = (Stage::Commit, Progress::Started);
    let host = Evidence::HostFailure;
    assert_failed(&failure, &acquired, commit, host, Teardown::Terminated);
    assert_eq!(failure.outcome(), Outcome::Host);
    assert_launch_ended(&fixture, &mut acquired, &vmm);
    // The guest ended the clone's VM with the status of a failed repair as the caller's step
    // failed: the status prevails, as over the sequence's own failures.
    let mut acquired = fixture.acquire("repair");
    let mut vmm = vmm_with("flush report:guest-exit:195");
    let (restoring, _) = launch(&fixture, &mut acquired, &image, &mut vmm);
    let mut control = FakeControl::default();
    let activated = restoring
        .activate(&mut control, RestoreDeadlines::default())
        .unwrap();
    vmm.end_and_wait();
    let failure = activated.fail(Evidence::InvalidMappings, Error::unsupported(mapping));
    let repair = Evidence::RepairStatus;
    assert_failed(&failure, &acquired, ready, repair, Teardown::Exited);
    assert_eq!(failure.record().status, Some(195));
    assert_launch_ended(&fixture, &mut acquired, &vmm);
    // Evidence of the template, such as a defect of the generation that the caller diagnosed,
    // shows the provenance that the status alone leaves to establish: it prevails, and the record
    // holds the status beside it. The failure quarantines the generation once the sandbox's lock
    // is released, as the lock order asks.
    let mut acquired = fixture.acquire("defect");
    let mut vmm = vmm_with("flush report:guest-exit:195");
    let (restoring, _) = launch(&fixture, &mut acquired, &image, &mut vmm);
    let mut control = FakeControl::default();
    let activated = restoring
        .activate(&mut control, RestoreDeadlines::default())
        .unwrap();
    vmm.end_and_wait();
    let defect = "the test diagnosed a defect of the generation's repair";
    let failure = activated.fail(Evidence::TemplateDefect, Error::other(defect));
    let template = Evidence::TemplateDefect;
    assert_failed(&failure, &acquired, ready, template, Teardown::Exited);
    assert_eq!(
        (failure.outcome(), failure.record().status),
        (Outcome::Template, Some(195))
    );
    assert_launch_ended(&fixture, &mut acquired, &vmm);
    drop(acquired);
    assert!(failure.quarantine(&fixture.store, None).unwrap());
    assert_eq!(
        fixture.store.tombstones().unwrap(),
        [failure.record().template.generation]
    );
}

#[test]
fn a_failure_that_cannot_be_recorded_before_its_teardown_keeps_its_launch_for_a_reattach() {
    let (fixture, image) = fixture();
    let mut acquired = fixture.acquire("unrecorded");
    let path = acquired.locked.clone_failure_path();
    let mut vmm = vmm_with("flush");
    let (restoring, _) = launch(&fixture, &mut acquired, &image, &mut vmm);
    let mut control = FakeControl::default();
    let activated = restoring
        .activate(&mut control, RestoreDeadlines::default())
        .unwrap();
    // The caller diagnoses a defect of the template once the clone is ready, but the record of
    // its failure cannot be written, as on a full disk: a directory takes its place.
    fs::create_dir(&path).unwrap();
    let defect = "the test diagnosed a defect of the generation's repair";
    let failure = activated.fail(Evidence::TemplateDefect, Error::other(defect));
    let ready = (Stage::Start, Progress::Ready);
    let record = failure.record();
    assert_eq!(
        ((record.stage, record.progress), record.evidence),
        (ready, Evidence::TemplateDefect)
    );
    // The caller holds the failure, the template's, whose teardown ended the clone's VMM through
    // its handle, and kept the launch's journal, and with it the lease.
    assert_eq!(failure.outcome(), Outcome::Template);
    let Teardown::Unresolved(reason) = &record.teardown else {
        panic!("the teardown of {failure} was resolved");
    };
    let unrecorded = "the failure could not be recorded before its teardown: ";
    let ended = format!("the clone's VMM ended through its handle, but {unrecorded}");
    assert!(reason.starts_with(&ended), "{reason}");
    assert!(failure.message().starts_with(defect), "{failure}");
    let lost = "; the failure could not be recorded: ";
    assert!(failure.message().contains(lost), "{failure}");
    assert_vmm_ended(&vmm);
    let journal = acquired.locked.journal().unwrap();
    let journal = journal.expect("the launch kept its journal");
    assert_eq!(journal.process(), vmm.reference.as_ref());
    let refusal = fixture.store.release(&mut acquired.locked).unwrap_err();
    assert_eq!(refusal.code(), ErrorCode::SandboxRunning, "{refusal}");
    // Once the record can be written, a reattach, as after a crash of the caller, finds that the
    // clone failed, though not how, rather than a clone that ended.
    fs::remove_dir(&path).unwrap();
    let mut control = FakeControl::default();
    let reattached = reattach_clone(
        &mut acquired.locked,
        &runtime_of(&fixture),
        &mut control,
        Instant::now() + TIMEOUT,
        TIMEOUT,
    )
    .unwrap();
    let resolved = failed(reattached);
    let exited = Teardown::Exited;
    assert_failed(&resolved, &acquired, ready, Evidence::LostStatus, exited);
    assert!(control.calls.is_empty());
    assert_launch_ended(&fixture, &mut acquired, &vmm);
}

#[test]
fn a_diagnosis_too_long_for_a_record_keeps_its_ends_and_its_evidence_through_a_reattach() {
    let (fixture, image) = fixture();
    let mut acquired = fixture.acquire("diagnosis");
    let mut vmm = vmm_with("flush report:guest-exit:195");
    let (restoring, _) = launch(&fixture, &mut acquired, &image, &mut vmm);
    let mut control = FakeControl::default();
    let activated = restoring
        .activate(&mut control, RestoreDeadlines::default())
        .unwrap();
    vmm.end_and_wait();
    // The caller's diagnosis of a defect of the generation is longer than a record keeps: JSON
    // takes six bytes for each of its control characters, the most that a character takes.
    let (first, last) = (
        "the test's diagnosis begins",
        "and the test's diagnosis ends",
    );
    let defect = format!("{first}{}{last}", "\u{1}".repeat(64 * 1024));
    let failure = activated.fail(Evidence::TemplateDefect, Error::other(defect.clone()));
    // The failure's error keeps the diagnosis whole, and its record, which the sandbox keeps,
    // its ends.
    assert_eq!(failure.message(), defect);
    let record = failure.record();
    let ready = (Stage::Start, Progress::Ready);
    assert_eq!(
        (
            (record.stage, record.progress),
            record.evidence,
            &record.teardown
        ),
        (ready, Evidence::TemplateDefect, &Teardown::Exited)
    );
    assert!(record.message.starts_with(first) && record.message.ends_with(last));
    assert!(record.message.chars().count() < defect.chars().count());
    let records = CloneRecords::read(&acquired.locked).unwrap().unwrap();
    assert_eq!(records.failure.as_ref(), Some(record));
    // A reattach finds the failure as it was recorded, with the template's evidence, which
    // quarantines the generation once the sandbox's lock is released.
    let mut control = FakeControl::default();
    let reattached = reattach_clone(
        &mut acquired.locked,
        &runtime_of(&fixture),
        &mut control,
        Instant::now() + TIMEOUT,
        TIMEOUT,
    )
    .unwrap();
    let again = failed(reattached);
    assert_eq!(again.record(), record);
    assert_launch_ended(&fixture, &mut acquired, &vmm);
    drop(acquired);
    assert!(again.quarantine(&fixture.store, None).unwrap());
    assert_eq!(
        fixture.store.tombstones().unwrap(),
        [record.template.generation]
    );
}

#[test]
fn a_clone_with_an_export_gets_its_mapping_table_once_ready_and_its_start_once_it_applied_it() {
    let (fixture, image) = fixture();
    let export = fixture.data_export("data");
    let request = with_export(tmpfs_request(&image.0, &image.1), &export);
    let mut acquired = fixture.acquire("exported");
    let mut vmm = vmm_with("flush");
    let (restoring, _) = launch_request(
        &fixture,
        &mut acquired,
        &request,
        &mut vmm,
        Hooks::LAUNCH.end,
    );
    let mut control = FakeControl::default();
    let activated = restoring
        .activate(&mut control, RestoreDeadlines::default())
        .unwrap();
    let deadline = activated.deadline();
    let started = activated.start(&mut control).unwrap();
    // In the session in which the clone answered, the table follows its Ready, in order, and the
    // start follows once the guest applied every mapping, both by the Ready-to-Start deadline.
    // The table names each path by its child's index, as a cold launch's does, through the
    // `MAPS` payloads that carry it.
    control.assert_activates_first();
    assert_eq!(data_table().to_vec(), export.guest_table());
    assert_eq!(
        control.calls[2..],
        [
            Call::Map {
                session: 1,
                table: data_table().to_vec(),
                deadline,
            },
            Call::Start {
                session: 1,
                deadline,
            },
        ]
    );
    assert_eq!(control.applied, data_table());
    let (running, _) = started.commit().unwrap();
    let locked = &mut acquired.locked;
    assert_eq!(
        running.terminate(locked, TIMEOUT).unwrap(),
        StopOutcome::Forced
    );
    assert_eq!(
        fixture.store.release(locked).unwrap(),
        Some(acquired.lease.clone())
    );

    // A clone without an export gets its start at once.
    let mut acquired = fixture.acquire("plain");
    let mut vmm = vmm_with("flush");
    let (restoring, _) = launch(&fixture, &mut acquired, &image, &mut vmm);
    assert!(restoring.table.is_empty());
    let mut control = FakeControl::default();
    let activated = restoring
        .activate(&mut control, RestoreDeadlines::default())
        .unwrap();
    let deadline = activated.deadline();
    let started = activated.start(&mut control).unwrap();
    assert_eq!(
        control.calls[2..],
        [Call::Start {
            session: 1,
            deadline,
        }]
    );
    assert!(control.applied.is_empty());
    drop(started.abandon().unwrap());
    assert_launch_ended(&fixture, &mut acquired, &vmm);
}

#[test]
fn a_partial_or_failed_mapping_table_ends_the_launch_and_its_sandbox_never_starts() {
    let (fixture, image) = fixture();
    let export = fixture.data_export("data");
    let request = with_export(tmpfs_request(&image.0, &image.1), &export);
    let deadlines = RestoreDeadlines {
        start: SHORT,
        ..RestoreDeadlines::default()
    };
    let refused = "the clone refused its mapping table";
    let failed = "the clone did not apply its mapping table";
    let lost = "the clone's control session was lost: the clone did not apply its mapping table";
    let late = "the clone did not apply its mapping table by its Ready-to-Start deadline";
    // A guest that refuses the table once it applied some of it, or none of it, as the request's
    // mappings decide; a failure of the framing that tells no refusal; a session lost once the
    // table was sent; and a guest that never answers.
    let cases = [
        (
            "partial",
            1,
            Answer::Fail(Failure::TABLE),
            (ErrorKind::Protocol, refused),
            Evidence::InvalidMappings,
        ),
        (
            "refused",
            0,
            Answer::Fail(Failure::TABLE),
            (ErrorKind::Protocol, refused),
            Evidence::InvalidMappings,
        ),
        (
            "failed",
            1,
            Answer::Fail(Failure::REFUSED),
            (ErrorKind::Protocol, failed),
            Evidence::Protocol,
        ),
        (
            "lost",
            1,
            Answer::Fail(Failure::LOST),
            (ErrorKind::Protocol, lost),
            Evidence::Reset,
        ),
        (
            "silent",
            0,
            Answer::Silent,
            (ErrorKind::TimedOut, late),
            Evidence::Timeout,
        ),
    ];
    // The progress log records the start only once the guest applied the whole table, so each
    // failure is recorded at the start's stage, with the clone's progress still at its Ready.
    let ready = (Stage::Start, Progress::Ready);
    for (name, applied, answer, (kind, message), evidence) in cases {
        let mut acquired = fixture.acquire(name);
        let mut vmm = vmm_with("flush");
        let (restoring, _) = launch_request(
            &fixture,
            &mut acquired,
            &request,
            &mut vmm,
            Hooks::LAUNCH.end,
        );
        let mut control = FakeControl::default();
        let source = match &answer {
            Answer::Fail(failure) => failure.to_string(),
            _ => Failure::REFUSED.to_string(),
        };
        control.maps.push_back((applied, answer));
        let activated = restoring.activate(&mut control, deadlines).unwrap();
        let deadline = activated.deadline();
        let error = activated.start(&mut control).unwrap_err();
        assert_eq!(error.kind(), kind, "{name}: {error}");
        assert_eq!(error.message(), message, "{name}");
        let failed = std::error::Error::source(&error).unwrap().to_string();
        assert_eq!(failed, source, "{name}");
        if kind == ErrorKind::TimedOut {
            assert!(Instant::now() >= deadline, "{name}");
        }
        // The table was sent once, in the session in which the clone answered, and no start, nor
        // another session, followed it.
        assert_eq!(control.sessions, 1, "{name}");
        assert_eq!(
            control.calls[2..],
            [Call::Map {
                session: 1,
                table: data_table().to_vec(),
                deadline,
            }],
            "{name}"
        );
        assert_eq!(control.applied, data_table()[..applied], "{name}");
        // The launch ended, through the VMM's verified identity, and the sandbox records the
        // failure, which is never the template's, so that none quarantines its generation.
        assert_failed(&error, &acquired, ready, evidence, Teardown::Terminated);
        assert!(!error.outcome().quarantines(), "{name}");
        assert_launch_ended(&fixture, &mut acquired, &vmm);
    }

    // The guest's refusal of the table is the request's whenever it came, even past the deadline.
    let mut acquired = fixture.acquire("late");
    let mut vmm = vmm_with("flush");
    let (restoring, _) = launch_request(
        &fixture,
        &mut acquired,
        &request,
        &mut vmm,
        Hooks::LAUNCH.end,
    );
    let mut control = FakeControl::default();
    control.maps.push_back((0, Answer::Fail(Failure::TABLE)));
    let passed = RestoreDeadlines {
        start: Duration::ZERO,
        ..RestoreDeadlines::default()
    };
    let activated = restoring.activate(&mut control, passed).unwrap();
    let error = activated.start(&mut control).unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (ErrorKind::Protocol, refused)
    );
    assert_failed(
        &error,
        &acquired,
        ready,
        Evidence::InvalidMappings,
        Teardown::Terminated,
    );
    assert_launch_ended(&fixture, &mut acquired, &vmm);

    // The guest ended the clone's VM as its table failed, with the status of an export that does
    // not mount: no start was sent, so no workload ran, and the status is evidence, which prevails
    // over the loss of the session.
    let mut acquired = fixture.acquire("status");
    let mut vmm = vmm_with("flush report:guest-exit:56");
    let (restoring, _) = launch_request(
        &fixture,
        &mut acquired,
        &request,
        &mut vmm,
        Hooks::LAUNCH.end,
    );
    let mut control = FakeControl::default();
    control.maps.push_back((0, Answer::Fail(Failure::LOST)));
    let activated = restoring
        .activate(&mut control, RestoreDeadlines::default())
        .unwrap();
    vmm.end_and_wait();
    let error = activated.start(&mut control).unwrap_err();
    assert_eq!(error.message(), lost);
    assert_failed(
        &error,
        &acquired,
        ready,
        Evidence::InvalidMappings,
        Teardown::Exited,
    );
    assert_eq!(error.record().status, Some(56));
    assert_launch_ended(&fixture, &mut acquired, &vmm);
}
