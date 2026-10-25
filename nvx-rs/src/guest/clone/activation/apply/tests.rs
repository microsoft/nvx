//! Tests of the application of a clone's activation record, through a fake system, which records
//! every call of the application, and whose assembly returns a temporary directory that stands
//! for the root, the image's view through the overlay, whose account databases the application
//! reads and checks for real; and, with the clone's activation, through an adapter of the tests'
//! own, which takes what the host sends, applies the record of the Activate that the activation
//! accepted through the fake system, and spawns an execution for each request that the
//! activation serves.

use std::cell::Cell;
use std::error::Error as _;
use std::fs;
use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::num::NonZeroU64;
use std::rc::Rc;
use std::time::Instant;

use super::*;
use crate::activation::{
    ExpectedResources, Export, ExportKind, MachineIdMode, Network, Publication, ReadyActivation,
    RestoreSection, StoreGeneration, TemplateReference, TransactionId, WorkloadView,
};
use crate::guest::clone::{
    Activation, ActivationAction, ActivationCause, ActivationFailure, ActivationInput,
    ActivationState, AdmissionFence,
};
use crate::guest::entry::{AccountError, AccountMode, Capabilities, Database};
use crate::protocol::root::{DISTRO_MMIO_BASE, SCRATCH_MMIO_BASE};
use crate::uuid::Uuid;

/// The runtime directory of the tests' template.
const RUNTIME: &str = "/run/nvx";
/// How long the tests' assembly waits for each device.
const DEVICE_TIMEOUT: Duration = Duration::from_secs(5);
/// The number of online CPUs of the tests' clone.
const CPUS: u32 = 2;
/// The memory size of the tests' clone: eight online memory blocks of 128 MiB.
const MEMORY: u64 = 8 << 27;
/// How long the tests' activation may take after the restore's acknowledgement.
const TIMEOUT: Duration = Duration::from_secs(10);
/// The transaction of the tests' Activate.
const TRANSACTION: TransactionId = TransactionId::from_bytes([0x22; TransactionId::LEN]);
/// The UUID that the record of the tests' Activate expects of its EROFS image.
const IMAGE_UUID: Uuid = Uuid::from_bytes([0x5a; 16]);
/// The tests' workload identity, whose account the tests' image holds.
const WORKLOAD: WorkloadIdentity = WorkloadIdentity {
    uid: 1000,
    gid: 1001,
};
/// The passwd database of the tests' image, which holds the workload identity's account.
const PASSWD: &[u8] = b"root:x:0:0:root:/root:/bin/sh\n\
workload:x:1000:1001::/home/workload:/bin/sh\n";
/// The group database of the tests' image, which holds the workload identity's group.
const GROUP: &[u8] = b"root:x:0:\nworkload:x:1001:\n";

/// Returns whether a failure's cause is the one that a test expects.
type Expected = fn(&ActivationCause) -> bool;

/// Returns whether an account's refusal is the one that a test expects.
type Refusal = fn(&AccountError) -> bool;

/// What a clone did, in order: each call that the application made of the fake system, and each
/// execution that the tests' adapter spawned.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Effect {
    /// The application assembled the root of this assembly, which mounts it.
    Assemble(Assembly),
    /// The application checked this identity's account in the root, which reads the image's
    /// account databases.
    Account(Identity),
    /// The application tore the root down, which unmounts it.
    Teardown,
    /// The adapter spawned an execution.
    Spawn,
}

/// A system that records each call of the application, and whose assembly returns the directory
/// of its image, unless it fails as a real one does: the image's view through the overlay, whose
/// `etc` holds the image's account databases.
struct FakeSystem {
    /// The directory that stands for the assembled root.
    image: tempfile::TempDir,
    /// Whether each assembly fails.
    failing: bool,
    /// What the clone did, in order.
    effects: Vec<Effect>,
}

impl FakeSystem {
    /// Returns a system whose image's `etc` holds `passwd` and `group`.
    fn with(passwd: &[u8], group: &[u8]) -> Self {
        let image = tempfile::tempdir().unwrap();
        let etc = image.path().join("etc");
        fs::create_dir(&etc).unwrap();
        fs::write(etc.join("passwd"), passwd).unwrap();
        fs::write(etc.join("group"), group).unwrap();
        Self {
            image,
            failing: false,
            effects: Vec::new(),
        }
    }

    /// Returns a system whose image holds the tests' account databases.
    fn new() -> Self {
        Self::with(PASSWD, GROUP)
    }

    /// Returns a system whose image holds the tests' account databases, and each of whose
    /// assemblies fails.
    fn failing() -> Self {
        Self {
            failing: true,
            ..Self::new()
        }
    }

    /// Returns the path of the root that each assembly returns.
    fn root(&self) -> PathBuf {
        self.image.path().to_owned()
    }
}

impl System for FakeSystem {
    type Root = PathBuf;

    fn assemble(&mut self, assembly: &Assembly) -> Result<PathBuf, AssemblyError> {
        self.effects.push(Effect::Assemble(assembly.clone()));
        if self.failing {
            // An assembly refuses a relative runtime directory before it does anything else,
            // which gives the fake a real failure.
            let refused = Assembly {
                runtime: PathBuf::from("run/nvx"),
                ..assembly.clone()
            };
            return Err(root::assemble(&refused).unwrap_err());
        }
        Ok(self.root())
    }

    fn account(&mut self, root: &PathBuf, identity: &Identity) -> Result<(), entry::Error> {
        self.effects.push(Effect::Account(identity.clone()));
        check_account(root, identity)
    }

    fn teardown(&mut self, root: PathBuf) -> Result<(), TeardownError> {
        assert_eq!(root, self.root());
        self.effects.push(Effect::Teardown);
        Ok(())
    }
}

/// The caller's fence of workload admission, which a restore handed over closed, and which the
/// tests' adapter checks before it spawns an execution.
#[derive(Clone, Default)]
struct FakeFence {
    /// Whether the fence opened.
    opened: Rc<Cell<bool>>,
}

impl FakeFence {
    /// Returns whether the fence is open.
    fn is_open(&self) -> bool {
        self.opened.get()
    }
}

impl AdmissionFence for FakeFence {
    fn close(&mut self) -> io::Result<()> {
        unreachable!("an activation never closes the fence");
    }

    fn open(&mut self) {
        self.opened.set(true);
    }
}

/// A clone's adapter in the tests, which takes what the host sends, as its session receives it,
/// through the clone's activation, behind a fake fence: it applies the record of the Activate that
/// the activation accepted through a fake system, with the tests' template, reports the result,
/// and spawns an execution for each request that the activation serves, once the fence is open.
struct Adapter {
    /// The clone's activation.
    activation: Activation<FakeFence>,
    /// The fence that the activation holds.
    fence: FakeFence,
    /// The system through which the adapter applies the record, which records what the clone did.
    system: FakeSystem,
    /// What the application activated: the root, and the identity in which every execution runs,
    /// if any.
    applied: Option<(PathBuf, Option<Identity>)>,
    /// What the adapter's latest Ready reported.
    ready: Option<ReadyActivation>,
}

impl Adapter {
    /// Returns the adapter of a clone, in its transport bootstrap, which applies records through
    /// `system`, and whose repair acknowledged its restore at `acknowledged`.
    fn new(system: FakeSystem, acknowledged: Instant) -> Self {
        let fence = FakeFence::default();
        let resources = ExpectedResources {
            cpus: CPUS,
            memory: MEMORY,
        };
        let activation = Activation::with_resources(
            resources,
            Some(acknowledged),
            fence.clone(),
            TIMEOUT,
            acknowledged,
        );
        Self {
            activation,
            fence,
            system,
            applied: None,
            ready: None,
        }
    }

    /// Returns the adapter of [`new`](Self::new) once the session's reset at `at` restricted its
    /// activation.
    fn restricted(system: FakeSystem, at: Instant) -> Self {
        let mut adapter = Self::new(system, at);
        adapter.take(at, ActivationInput::Reset).unwrap();
        assert_eq!(adapter.activation.state(), ActivationState::Restricted);
        adapter
    }

    /// Takes `input`, which the session received at `now`, as an adapter does, or returns the
    /// activation's failure.
    fn take(&mut self, now: Instant, input: ActivationInput<'_>) -> Result<(), ActivationFailure> {
        match self.activation.input(now, input)? {
            ActivationAction::Acknowledge | ActivationAction::Continue | ActivationAction::Join => {
            }
            ActivationAction::Apply(record) => {
                let result = match apply_with(&mut self.system, &record, &template()) {
                    Ok(applied) => {
                        self.applied = Some(applied);
                        Ok(())
                    }
                    Err(error) => Err(error.into()),
                };
                self.ready = Some(self.activation.complete(now, result)?);
            }
            ActivationAction::Ready(ready) => self.ready = Some(ready),
            ActivationAction::Serve => {
                assert!(
                    self.fence.is_open(),
                    "an execution spawned behind the fence"
                );
                self.system.effects.push(Effect::Spawn);
            }
        }
        Ok(())
    }

    /// Checks that the clone failed with `failure`, whose cause `expected` accepts, and did
    /// nothing: it mounted nothing, read nothing, and spawned nothing, reported no Ready, and its
    /// fence is closed.
    fn assert_untouched(&self, failure: &ActivationFailure, expected: Expected) {
        assert!(expected(failure.cause()), "{failure}");
        assert_eq!(self.activation.state(), ActivationState::Failed);
        assert!(self.system.effects.is_empty(), "{:?}", self.system.effects);
        assert_eq!(self.applied, None);
        assert_eq!(self.ready, None);
        assert!(!self.fence.is_open());
    }
}

/// Returns the tests' template: its runtime directory, its devices at NVX's MMIO windows, with
/// the sizes that they must have, and its timeout.
fn template() -> RootTemplate {
    RootTemplate {
        runtime: PathBuf::from(RUNTIME),
        distro: Device {
            mmio: DISTRO_MMIO_BASE,
            sectors: Some(1 << 21),
        },
        scratch: Device {
            mmio: SCRATCH_MMIO_BASE,
            sectors: Some(1 << 22),
        },
        timeout: DEVICE_TIMEOUT,
    }
}

/// Returns an EROFS image, with the UUID `uuid` that the record expects of it, if any.
fn erofs(uuid: Option<Uuid>) -> activation::Image {
    activation::Image::Erofs {
        binding: activation::Identity::from_bytes([0x11; activation::Identity::LEN]),
        uuid,
    }
}

/// Returns a GPT disk, without a GUID that the record expects of it.
fn gpt() -> activation::Image {
    activation::Image::Gpt {
        binding: activation::Identity::from_bytes([0x11; activation::Identity::LEN]),
        disk_guid: None,
        first_partition_bound: 4096,
    }
}

/// Returns the scratch upper, with the journal policy `journal`.
fn scratch(journal: activation::Journal) -> activation::Upper {
    activation::Upper::Scratch {
        binding: activation::Identity::from_bytes([0x33; activation::Identity::LEN]),
        journal,
    }
}

/// Returns the configuration of a root of `image` over `upper`, in `overlay` mode, whose workload
/// runs as `identity`, if any, which expects the tests' clone's resources, without an export or
/// a network.
fn configuration(
    image: activation::Image,
    upper: activation::Upper,
    overlay: activation::OverlayMode,
    identity: Option<WorkloadIdentity>,
) -> Configuration {
    Configuration {
        image,
        upper,
        overlay,
        export: None,
        mappings: 0,
        network: None,
        workload: WorkloadView {
            hostname: b"clone".to_vec(),
            machine_id: MachineIdMode::Image,
            identity,
        },
        resources: ExpectedResources {
            cpus: CPUS,
            memory: MEMORY,
        },
    }
}

/// Returns the record of a clone's restore of the tests' transaction, of `configuration`.
fn clone_record(configuration: Configuration) -> ActivationRecord {
    let restore = RestoreSection {
        transaction: TRANSACTION,
        template: TemplateReference {
            generation: StoreGeneration::new(NonZeroU64::MIN),
            key: activation::Identity::from_bytes([0x44; activation::Identity::LEN]),
        },
    };
    ActivationRecord::new(Some(restore), configuration).unwrap()
}

/// Returns the record of the tests' Activate: an EROFS image, whose UUID it expects, over a
/// tmpfs, with untrusted lowers, whose workload runs as the tests' workload identity.
fn accepted() -> ActivationRecord {
    clone_record(configuration(
        erofs(Some(IMAGE_UUID)),
        activation::Upper::Tmpfs,
        activation::OverlayMode::UntrustedLowers,
        Some(WORKLOAD),
    ))
}

/// Returns the assembly of the root of the tests' Activate: its EROFS image, with the UUID that
/// the record expects of it, over a tmpfs, with untrusted lowers, with the template's runtime
/// directory, devices, and timeout.
fn accepted_assembly() -> Assembly {
    let template = template();
    Assembly {
        runtime: template.runtime,
        distro: template.distro,
        image: root::Image::Erofs {
            uuid: Some(IMAGE_UUID),
        },
        upper: root::Upper::Tmpfs,
        overlay: root::OverlayMode::UntrustedLowers,
        timeout: template.timeout,
    }
}

/// Returns the identity in which the executions of the tests' workload run.
fn workload() -> Identity {
    Identity::new(WORKLOAD.uid, WORKLOAD.gid)
}

/// Returns the application's error that `failure`, an activation's, holds.
fn apply_error(failure: &ActivationFailure) -> &ApplyError {
    match failure.cause() {
        ActivationCause::Failed(error) => error.downcast_ref::<ApplyError>().unwrap(),
        cause => panic!("{cause}"),
    }
}

#[test]
fn no_mount_spawn_or_id_generation_happens_before_the_gate() {
    // The application reaches the system only through its calls, which the fake records in
    // full: none starts a process or generates an ID, and the adapter spawns each execution
    // itself, once the activation serves its request.
    let base = Instant::now();
    let record = accepted().encode();
    let settings = accepted().configuration().clone();
    let cold = ActivationRecord::new(None, settings.clone())
        .unwrap()
        .encode();
    let larger = clone_record(Configuration {
        resources: ExpectedResources {
            cpus: CPUS,
            memory: MEMORY + (1 << 27),
        },
        ..settings.clone()
    })
    .encode();
    let disk = clone_record(Configuration {
        image: gpt(),
        ..settings
    })
    .encode();

    // Before the session's reset, the host can send nothing.
    let mut adapter = Adapter::new(FakeSystem::new(), base);
    adapter.take(base, ActivationInput::Flow).unwrap();
    let failure = adapter
        .take(base, ActivationInput::Activate(&record))
        .unwrap_err();
    adapter.assert_untouched(&failure, |cause| matches!(cause, ActivationCause::Request));
    let mut adapter = Adapter::new(FakeSystem::new(), base);
    let failure = adapter.take(base, ActivationInput::Request).unwrap_err();
    adapter.assert_untouched(&failure, |cause| matches!(cause, ActivationCause::Request));

    // Once the session is restricted, a request or another reset fails, and so does an Activate
    // whose record does not decode, is a cold boot's, or expects other resources.
    let refused: [(ActivationInput<'_>, Expected); 5] = [
        (ActivationInput::Request, |cause| {
            matches!(cause, ActivationCause::Request)
        }),
        (ActivationInput::Reset, |cause| {
            matches!(cause, ActivationCause::Reset)
        }),
        (
            ActivationInput::Activate(&record[..record.len() - 1]),
            |cause| matches!(cause, ActivationCause::Record(_)),
        ),
        (ActivationInput::Activate(&cold), |cause| {
            matches!(cause, ActivationCause::Cold)
        }),
        (ActivationInput::Activate(&larger), |cause| {
            matches!(cause, ActivationCause::Resources { .. })
        }),
    ];
    for (input, expected) in refused {
        let mut adapter = Adapter::restricted(FakeSystem::new(), base);
        adapter.take(base, ActivationInput::Flow).unwrap();
        let failure = adapter.take(base, input).unwrap_err();
        adapter.assert_untouched(&failure, expected);
    }

    // The application refuses a GPT disk, which the activation accepted, before it mounts
    // anything.
    let mut adapter = Adapter::restricted(FakeSystem::new(), base);
    let failure = adapter
        .take(base, ActivationInput::Activate(&disk))
        .unwrap_err();
    adapter.assert_untouched(&failure, |cause| {
        matches!(cause, ActivationCause::Failed(error)
            if matches!(error.downcast_ref::<ApplyError>(), Some(ApplyError::Gpt)))
    });
    assert_eq!(failure.state(), ActivationState::Activating);

    // Past the deadline, even a valid Activate does nothing.
    let mut adapter = Adapter::restricted(FakeSystem::new(), base);
    let failure = adapter
        .take(base + TIMEOUT, ActivationInput::Activate(&record))
        .unwrap_err();
    adapter.assert_untouched(&failure, |cause| matches!(cause, ActivationCause::Expired));

    // Only a valid Activate mounts the root, and reads its account databases, before Ready, and
    // an execution spawns only once Ready opened the fence.
    let mut adapter = Adapter::restricted(FakeSystem::new(), base);
    adapter.take(base, ActivationInput::Flow).unwrap();
    assert!(adapter.system.effects.is_empty());
    adapter
        .take(base, ActivationInput::Activate(&record))
        .unwrap();
    assert_eq!(
        adapter.system.effects,
        [
            Effect::Assemble(accepted_assembly()),
            Effect::Account(workload()),
        ]
    );
    assert!(adapter.fence.is_open());
    adapter.take(base, ActivationInput::Request).unwrap();
    assert_eq!(adapter.system.effects.last(), Some(&Effect::Spawn));
}

#[test]
fn a_valid_activate_activates_the_root_and_the_workload_once_before_ready() {
    let base = Instant::now();
    let record = accepted().encode();
    let mut adapter = Adapter::restricted(FakeSystem::new(), base);
    adapter
        .take(base, ActivationInput::Activate(&record))
        .unwrap();
    // The application assembled the root with the record's settings, and checked the workload
    // identity's account in it, before Ready, which reports that the activation applied, with the
    // record's transaction.
    assert_eq!(adapter.activation.state(), ActivationState::Ready);
    assert_eq!(
        adapter.ready,
        Some(ReadyActivation {
            transaction: Some(TRANSACTION),
        })
    );
    assert_eq!(
        adapter.applied,
        Some((adapter.system.root(), Some(workload())))
    );
    assert!(adapter.fence.is_open());
    // Every execution runs as the workload identity, which has no supplementary groups or
    // capabilities, and whose account an entry checks in existing-account mode.
    let identity = workload();
    assert_eq!((identity.uid(), identity.gid()), (1000, 1001));
    assert!(identity.supplementary_groups().is_empty());
    assert_eq!(identity.capability_set(), Capabilities::EMPTY);
    assert_eq!(identity.account_mode(), AccountMode::Existing);

    // A later session, and a retry of the Activate in it, get the established activation
    // without applying the record again; requests spawn executions.
    adapter.take(base, ActivationInput::Request).unwrap();
    adapter.take(base, ActivationInput::Reset).unwrap();
    adapter.ready = None;
    adapter
        .take(base, ActivationInput::Activate(&record))
        .unwrap();
    assert_eq!(adapter.ready, Some(ReadyActivation::of(&accepted())));
    adapter.take(base, ActivationInput::Request).unwrap();
    assert_eq!(
        adapter.system.effects,
        [
            Effect::Assemble(accepted_assembly()),
            Effect::Account(workload()),
            Effect::Spawn,
            Effect::Spawn,
        ]
    );
}

#[test]
fn assembly_and_identity_inputs_are_taken_exactly_from_the_record() {
    let template = template();
    let images = [
        (erofs(None), None),
        (erofs(Some(IMAGE_UUID)), Some(IMAGE_UUID)),
    ];
    let uppers = [
        (activation::Upper::Tmpfs, root::Upper::Tmpfs),
        (
            scratch(activation::Journal::Allowed),
            root::Upper::Scratch {
                device: template.scratch,
                journal: root::Journal::Allowed,
            },
        ),
        (
            scratch(activation::Journal::Forbidden),
            root::Upper::Scratch {
                device: template.scratch,
                journal: root::Journal::Forbidden,
            },
        ),
    ];
    let overlays = [
        (
            activation::OverlayMode::TrustedLowers,
            root::OverlayMode::TrustedLowers,
        ),
        (
            activation::OverlayMode::UntrustedLowers,
            root::OverlayMode::UntrustedLowers,
        ),
    ];
    for (image, uuid) in images {
        for (upper, root_upper) in uppers {
            for (overlay, root_overlay) in overlays {
                for identity in [None, Some(WORKLOAD)] {
                    let record = clone_record(configuration(image, upper, overlay, identity));
                    let mut system = FakeSystem::new();
                    let (root, applied) = apply_with(&mut system, &record, &template).unwrap();
                    assert_eq!(root, system.root());
                    let assembly = Assembly {
                        runtime: template.runtime.clone(),
                        distro: template.distro,
                        image: root::Image::Erofs { uuid },
                        upper: root_upper,
                        overlay: root_overlay,
                        timeout: template.timeout,
                    };
                    let identity = identity.map(|_| workload());
                    let mut effects = vec![Effect::Assemble(assembly)];
                    effects.extend(identity.clone().map(Effect::Account));
                    assert_eq!(system.effects, effects, "{record:?}");
                    assert_eq!(applied, identity);
                }
            }
        }
    }

    // The record's other settings, which the application leaves alone, change neither input:
    // its bindings, export, network, hostname, machine ID, resources, and restore section.
    let mut other = accepted().configuration().clone();
    other.image = activation::Image::Erofs {
        binding: activation::Identity::from_bytes([0x66; activation::Identity::LEN]),
        uuid: Some(IMAGE_UUID),
    };
    other.export = Some(Export {
        kind: ExportKind::Aggregate,
        plan: activation::Identity::from_bytes([0x77; activation::Identity::LEN]),
    });
    other.mappings = 3;
    other.network = Some(Network {
        policy: activation::Identity::from_bytes([0x88; activation::Identity::LEN]),
        dns: Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))),
        publication: Publication::Published,
        proxy: Some("http://10.0.0.1:3128".to_owned()),
    });
    other.workload.hostname = b"another".to_vec();
    other.workload.machine_id = MachineIdMode::Generated;
    other.resources = ExpectedResources {
        cpus: 8,
        memory: 1 << 33,
    };
    let other = ActivationRecord::new(None, other).unwrap();
    let (mut first, mut second) = (FakeSystem::new(), FakeSystem::new());
    let first_identity = apply_with(&mut first, &accepted(), &template).unwrap().1;
    let second_identity = apply_with(&mut second, &other, &template).unwrap().1;
    assert_eq!(first.effects, second.effects);
    assert_eq!(first_identity, second_identity);
}

#[test]
fn a_workload_identity_without_an_account_in_the_image_is_refused_before_ready() {
    let base = Instant::now();
    let record = accepted().encode();
    // Images that hold no account of the workload identity's user ID whose primary group is its
    // group ID, which the group database holds, and whose home is absolute.
    let without_passwd = FakeSystem::new();
    fs::remove_file(without_passwd.image.path().join("etc/passwd")).unwrap();
    let systems: [(FakeSystem, Refusal); 5] = [
        (
            FakeSystem::with(b"root:x:0:0:root:/root:/bin/sh\n", GROUP),
            |error| matches!(error, AccountError::Missing),
        ),
        (
            FakeSystem::with(b"workload:x:1000:1002::/home/workload:/bin/sh\n", GROUP),
            |error| matches!(error, AccountError::PrimaryGroup(1002)),
        ),
        (FakeSystem::with(PASSWD, b"root:x:0:\n"), |error| {
            matches!(error, AccountError::Group)
        }),
        (
            FakeSystem::with(b"workload:x:1000:1001::home:/bin/sh\n", GROUP),
            |error| matches!(error, AccountError::Home),
        ),
        (without_passwd, |error| {
            matches!(error, AccountError::Read(Database::Passwd, _))
        }),
    ];
    for (system, refusal) in systems {
        let mut adapter = Adapter::restricted(system, base);
        let failure = adapter
            .take(base, ActivationInput::Activate(&record))
            .unwrap_err();
        assert_eq!(failure.state(), ActivationState::Activating);
        let ApplyError::Account {
            error: entry::Error::Account(error),
            teardown: None,
        } = apply_error(&failure)
        else {
            panic!("{failure}");
        };
        assert!(refusal(error), "{error}");
        // The root was assembled, its account databases read, and the root torn down, without
        // a Ready, and behind the closed fence.
        assert_eq!(
            adapter.system.effects,
            [
                Effect::Assemble(accepted_assembly()),
                Effect::Account(workload()),
                Effect::Teardown,
            ]
        );
        assert_eq!(adapter.activation.state(), ActivationState::Failed);
        assert_eq!(adapter.applied, None);
        assert_eq!(adapter.ready, None);
        assert!(!adapter.fence.is_open());
        // The failure is final: no retry applies the record again, and nothing is served.
        for input in [
            ActivationInput::Activate(&record),
            ActivationInput::Request,
            ActivationInput::Reset,
        ] {
            assert!(adapter.take(base, input).is_err());
        }
        assert_eq!(adapter.system.effects.len(), 3);
    }

    // Without a workload identity, the application reads no account: each execution names its
    // own identity, which the adapter's entry checks.
    let anyone = clone_record(configuration(
        erofs(Some(IMAGE_UUID)),
        activation::Upper::Tmpfs,
        activation::OverlayMode::UntrustedLowers,
        None,
    ));
    let mut system = FakeSystem::with(b"", b"");
    let (_, identity) = apply_with(&mut system, &anyone, &template()).unwrap();
    assert_eq!(identity, None);
    assert_eq!(system.effects, [Effect::Assemble(accepted_assembly())]);
}

#[test]
fn a_failed_assembly_is_final_and_reads_and_tears_down_nothing() {
    let base = Instant::now();
    let record = accepted().encode();
    let mut adapter = Adapter::restricted(FakeSystem::failing(), base);
    let failure = adapter
        .take(base, ActivationInput::Activate(&record))
        .unwrap_err();
    let ApplyError::Assembly(error) = apply_error(&failure) else {
        panic!("{failure}");
    };
    assert!(
        matches!(error.error(), root::Error::Runtime { path } if path == Path::new("run/nvx")),
        "{error}"
    );
    assert!(error.cleanup().is_none());
    assert_eq!(
        adapter.system.effects,
        [Effect::Assemble(accepted_assembly())]
    );
    assert_eq!(adapter.applied, None);
    assert_eq!(adapter.ready, None);
    assert!(!adapter.fence.is_open());
    assert!(
        adapter
            .take(base, ActivationInput::Activate(&record))
            .is_err()
    );
    assert_eq!(adapter.system.effects.len(), 1);
}

#[test]
fn the_application_refuses_without_privileges_what_it_cannot_assemble() {
    // A GPT disk is refused before anything is mounted, and a template's runtime directory
    // reaches the assembly, which refuses a relative one before it does anything else.
    let disk = clone_record(configuration(
        gpt(),
        activation::Upper::Tmpfs,
        activation::OverlayMode::TrustedLowers,
        Some(WORKLOAD),
    ));
    assert!(matches!(apply(&disk, &template()), Err(ApplyError::Gpt)));
    let relative = RootTemplate {
        runtime: PathBuf::from("run/nvx"),
        ..template()
    };
    let Err(ApplyError::Assembly(error)) = apply(&accepted(), &relative) else {
        panic!("a relative runtime directory was assembled");
    };
    assert!(
        matches!(error.error(), root::Error::Runtime { path } if path == Path::new("run/nvx")),
        "{error}"
    );
}

#[test]
fn application_errors_name_their_cause() {
    assert_eq!(
        ApplyError::Gpt.to_string(),
        "the record's image is a GPT disk, which a clone does not assemble"
    );
    assert!(ApplyError::Gpt.source().is_none());

    let relative = Assembly {
        runtime: PathBuf::from("run/nvx"),
        ..accepted_assembly()
    };
    let refusal = root::assemble(&relative).unwrap_err().to_string();
    let error = ApplyError::Assembly(root::assemble(&relative).unwrap_err());
    assert_eq!(
        error.to_string(),
        format!("the root's assembly failed: {refusal}")
    );
    assert_eq!(error.source().unwrap().to_string(), refusal);

    let error = ApplyError::Account {
        error: entry::Error::Account(AccountError::Missing),
        teardown: None,
    };
    assert_eq!(
        error.to_string(),
        "the root refuses the workload identity: the account is unavailable: no passwd entry has \
         the user ID"
    );
    assert_eq!(
        error.source().unwrap().to_string(),
        "the account is unavailable: no passwd entry has the user ID"
    );
    assert!(format!("{error:?}").starts_with("Account { error: Account(Missing)"));

    // The adapter hands the error to the activation, whose failure names it.
    let base = Instant::now();
    let mut adapter = Adapter::restricted(FakeSystem::with(b"", GROUP), base);
    let failure = adapter
        .take(base, ActivationInput::Activate(&accepted().encode()))
        .unwrap_err();
    assert_eq!(
        failure.to_string(),
        "the activation failed in its activating state: the record's application failed: the \
         root refuses the workload identity: the account is unavailable: no passwd entry has the \
         user ID"
    );
    assert!(failure.source().unwrap().is::<ApplyError>());
}
