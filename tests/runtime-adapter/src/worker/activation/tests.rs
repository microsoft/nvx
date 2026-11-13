use std::io;
use std::num::NonZeroU64;
use std::path::PathBuf;

use nvx::activation::{
    Configuration, ExpectedResources, Identity, Image, MachineIdMode, OverlayMode, RestoreSection,
    StoreGeneration, TemplateReference, TransactionId, Upper, WorkloadIdentity, WorkloadView,
};
use nvx::guest::clone::{AdmissionFence, NetworkError};
use nvx::guest::entry;
use nvx::guest::ingress::StartError;
use nvx::guest::mappings::MappingError;
use nvx::guest::root::{self, Assembly, Device, Error as RootError, Role};
use nvx::protocol::root::DISTRO_MMIO_BASE;

use super::super::capture::FILESYSTEM_TAG;
use super::*;

/// The resources of the clones of the tests.
const RESOURCES: ExpectedResources = ExpectedResources {
    cpus: 2,
    memory: 1 << 30,
};

/// Returns a clone's record of the transaction `transaction`, with the workload identity of
/// `uid`.
fn record(transaction: u8, uid: u32) -> ActivationRecord {
    let configuration = Configuration {
        image: Image::Erofs {
            binding: Identity::from_bytes([0x11; 32]),
            uuid: None,
        },
        upper: Upper::Tmpfs,
        overlay: OverlayMode::UntrustedLowers,
        export: None,
        mappings: 0,
        network: None,
        workload: WorkloadView {
            hostname: b"clone".to_vec(),
            machine_id: MachineIdMode::Image,
            identity: Some(WorkloadIdentity { uid, gid: uid }),
        },
        resources: RESOURCES,
    };
    let restore = RestoreSection {
        transaction: TransactionId::from_bytes([transaction; 16]),
        template: TemplateReference {
            generation: StoreGeneration::new(NonZeroU64::MIN),
            key: Identity::from_bytes([0x44; 32]),
        },
    };
    ActivationRecord::new(Some(restore), configuration).unwrap()
}

/// Returns a clone's activation, in its transport bootstrap, behind a closed fence, with
/// `resources`, which the deadline ends `timeout` after now.
fn activation(resources: ExpectedResources, timeout: Duration) -> Activation<Admission> {
    let mut fence = Admission::default();
    fence.close().unwrap();
    let now = Instant::now();
    Activation::with_resources(resources, Some(now), fence, timeout, now)
}

/// Returns an activation that took the session's reset, and is restricted.
fn restricted() -> Activation<Admission> {
    let mut activation = activation(RESOURCES, ACTIVATION_TIMEOUT);
    let action = activation.input(Instant::now(), ActivationInput::Reset);
    assert_eq!(action.unwrap(), ActivationAction::Acknowledge);
    activation
}

/// Returns an activation that accepted the record of the transaction 1.
fn activating() -> Activation<Admission> {
    let mut activation = restricted();
    let bytes = record(1, 1000).encode();
    let action = activation.input(Instant::now(), ActivationInput::Activate(&bytes));
    assert!(
        matches!(action, Ok(ActivationAction::Apply(_))),
        "{action:?}"
    );
    activation
}

/// Returns the failure of the activation with which `result` fails.
fn failed<T: std::fmt::Debug>(result: Result<T, ActivationFailure>) -> ActivationFailure {
    result.unwrap_err()
}

#[test]
fn each_class_of_failure_ends_the_clone_with_its_own_status() {
    let now = Instant::now;
    let bytes = record(1, 1000).encode();
    let mut cases = Vec::new();

    // The deadline.
    let mut expired = activation(RESOURCES, Duration::ZERO);
    cases.push((failed(expired.tick(now())), status::ACTIVATION_DEADLINE));
    // The host's session: another reset, a request, another transaction, a changed record.
    let mut reset = restricted();
    let failure = failed(reset.input(now(), ActivationInput::Reset));
    cases.push((failure, status::ACTIVATION_PROTOCOL));
    let mut request = restricted();
    let failure = failed(request.input(now(), ActivationInput::Request));
    cases.push((failure, status::ACTIVATION_PROTOCOL));
    let early = activation(RESOURCES, ACTIVATION_TIMEOUT).input(now(), ActivationInput::Request);
    cases.push((failed(early), status::ACTIVATION_PROTOCOL));
    let other = record(2, 1000).encode();
    let failure = failed(activating().input(now(), ActivationInput::Activate(&other)));
    assert!(matches!(
        failure.cause(),
        ActivationCause::Transaction { .. }
    ));
    cases.push((failure, status::ACTIVATION_PROTOCOL));
    let changed = record(1, 1001).encode();
    let failure = failed(activating().input(now(), ActivationInput::Activate(&changed)));
    assert!(matches!(failure.cause(), ActivationCause::Changed));
    cases.push((failure, status::ACTIVATION_PROTOCOL));
    // The record: one that does not decode, and a cold boot's.
    let failure = failed(restricted().input(now(), ActivationInput::Activate(b"\x01")));
    cases.push((failure, status::ACTIVATION_RECORD));
    let cold = ActivationRecord::new(None, record(1, 1000).configuration().clone()).unwrap();
    let cold = cold.encode();
    let failure = failed(restricted().input(now(), ActivationInput::Activate(&cold)));
    assert!(matches!(failure.cause(), ActivationCause::Cold));
    cases.push((failure, status::ACTIVATION_RECORD));
    // The resources.
    let mut more = activation(
        ExpectedResources {
            cpus: 4,
            ..RESOURCES
        },
        ACTIVATION_TIMEOUT,
    );
    more.input(now(), ActivationInput::Reset).unwrap();
    let failure = failed(more.input(now(), ActivationInput::Activate(&bytes)));
    cases.push((failure, status::ACTIVATION_RESOURCES));
    // The application: of the root, of the workload, of the export, of the network, and any
    // other.
    let reread = ApplyError::Gpt(RootError::Missing {
        role: Role::Distro,
        mmio: DISTRO_MMIO_BASE,
    });
    let root = activating().complete(now(), Err(reread.into()));
    cases.push((failed(root), status::ACTIVATION_ROOT));
    // An assembly refuses a relative runtime directory before anything else.
    let relative = Assembly {
        runtime: PathBuf::from("run/nvx"),
        distro: Device {
            mmio: DISTRO_MMIO_BASE,
            sectors: None,
        },
        image: root::Image::Erofs { uuid: None },
        upper: root::Upper::Tmpfs,
        overlay: root::OverlayMode::default(),
        timeout: Duration::ZERO,
    };
    let assembly = ApplyError::Assembly(root::assemble(&relative).unwrap_err());
    let root = activating().complete(now(), Err(assembly.into()));
    cases.push((failed(root), status::ACTIVATION_ROOT));
    let generated = ApplyError::MachineId {
        error: io::ErrorKind::AlreadyExists.into(),
        teardown: None,
    };
    let workload = activating().complete(now(), Err(generated.into()));
    cases.push((failed(workload), status::ACTIVATION_WORKLOAD));
    let view = ApplyError::View {
        error: entry::Error::Hostname,
        teardown: None,
    };
    let workload = activating().complete(now(), Err(view.into()));
    cases.push((failed(workload), status::ACTIVATION_WORKLOAD));
    let unmounted = ApplyError::Export {
        error: RootError::Mount {
            mount: root::Mount::Export {
                tag: FILESYSTEM_TAG.to_owned(),
            },
            error: io::ErrorKind::NotFound.into(),
        },
        teardown: None,
    };
    let export = activating().complete(now(), Err(unmounted.into()));
    cases.push((failed(export), status::ACTIVATION_EXPORT));
    let unwritten = ApplyError::Network {
        error: NetworkError::Write(io::ErrorKind::AlreadyExists.into()),
        teardown: None,
    };
    let network = activating().complete(now(), Err(unwritten.into()));
    cases.push((failed(network), status::ACTIVATION_NETWORK));
    let unbound = ApplyError::Network {
        error: NetworkError::Bind(entry::Error::Resolver(MappingError::TargetKind)),
        teardown: None,
    };
    let network = activating().complete(now(), Err(unbound.into()));
    cases.push((failed(network), status::ACTIVATION_NETWORK));
    for error in [
        NetworkError::Gateway,
        NetworkError::Announcer(StartError::NoPrivilegedPort),
    ] {
        let unannounced = ApplyError::Network {
            error,
            teardown: None,
        };
        let network = activating().complete(now(), Err(unannounced.into()));
        cases.push((failed(network), status::ACTIVATION_NETWORK));
    }
    let other = activating().complete(now(), Err("another failure of the application".into()));
    cases.push((failed(other), status::ACTIVATION_ROOT));
    // The worker's own order.
    let failure = failed(restricted().complete(now(), Ok(())));
    assert!(matches!(failure.cause(), ActivationCause::Completion));
    cases.push((failure, status::ACTIVATION_ORDER));
    let failure = failed(restricted().start());
    assert!(matches!(failure.cause(), ActivationCause::Start));
    cases.push((failure, status::ACTIVATION_ORDER));

    for (failure, expected) in &cases {
        assert_eq!(failure_status(failure), *expected, "{failure}");
        let ended = unactivated(failure);
        assert_eq!(ended.status(), *expected, "{failure}");
        assert_eq!(
            ended,
            Worked::Unactivated {
                status: *expected,
                problem: failure.to_string(),
            }
        );
    }
    // Every class that the adapter's clones reach has its status.
    let mut statuses = cases
        .iter()
        .map(|(failure, _)| failure_status(failure))
        .collect::<Vec<_>>();
    statuses.sort_unstable();
    statuses.dedup();
    assert_eq!(statuses, [50, 51, 52, 53, 54, 55, 56, 57, 58]);
    assert!(
        statuses
            .iter()
            .all(|status| status::ACTIVATION_STATUSES.contains(status))
    );
}

#[test]
fn the_activation_s_timeout_covers_the_wait_for_both_devices() {
    use super::super::capture::{root_template, template_config};

    let template = template_config();
    assert!(ACTIVATION_TIMEOUT >= 2 * template.device_timeout);
    assert_eq!(
        root_template(&template, None).timeout,
        template.device_timeout
    );
}

#[test]
fn the_activation_probe_reports_the_clone_s_events_after_the_acknowledgement() {
    let acknowledged = Instant::now();
    let after = |micros| acknowledged + Duration::from_micros(micros);
    let timeline = Timeline {
        acknowledged: Some(acknowledged),
        repair_delay: None,
        reset: Some(after(10)),
        message: Some(after(250)),
        ready: Some(after(4_000)),
        started: Some(after(4_100)),
        execution: None,
    };
    let mut expected = Facts::new();
    let facts = [
        ("acknowledged", "yes"),
        ("repair-delay", "none"),
        ("reset", "10"),
        ("message", "250"),
        ("ready", "4000"),
        ("started", "4100"),
        ("execution", "none"),
    ];
    for (key, value) in facts {
        expected.push(key, value);
    }
    assert_eq!(timeline.facts(), expected);
    let probe = Message::Probe(ACTIVATION_PROBE.as_bytes().to_vec());
    let report = Message::Report(expected.encode());
    assert_eq!(timeline.answer(&probe), Some(report));
    let with_argument = Message::Probe(b"activation now".to_vec());
    let problem = b"the activation probe takes no arguments".to_vec();
    assert_eq!(
        timeline.answer(&with_argument),
        Some(Message::ProbeFailed(problem))
    );
    // The probes of the root, and other messages, are the order's.
    for message in [
        Message::Probe(b"mounts".to_vec()),
        Message::Probe(b"activations".to_vec()),
        Message::Ping,
    ] {
        assert_eq!(timeline.answer(&message), None, "{message:?}");
    }

    // An event before the acknowledgement, and a timeline without one.
    let early = Timeline {
        acknowledged: Some(after(100)),
        ..timeline
    };
    assert_eq!(early.facts().get("reset"), Some("before"));
    assert_eq!(early.facts().get("message"), Some("150"));
    let unacknowledged = Timeline {
        acknowledged: None,
        ..timeline
    };
    let facts = unacknowledged.facts();
    assert_eq!(facts.get("acknowledged"), Some("none"));
    assert_eq!(facts.get("reset"), Some("unknown"));
    assert_eq!(facts.get("execution"), Some("none"));

    // A template that delayed the repair of its clones, for tests.
    let delayed = Timeline {
        repair_delay: Some(Duration::from_millis(8_000)),
        ..timeline
    };
    assert_eq!(delayed.facts().get("repair-delay"), Some("8000000"));
}
