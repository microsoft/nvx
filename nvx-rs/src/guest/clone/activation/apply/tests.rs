//! Tests of the application of a clone's activation record, through a fake system, which records
//! every call of the application, and whose assembly returns a temporary directory that stands
//! for the root, the image's view through the overlay, whose account databases the application
//! reads and checks for real, and which generates no machine ID, writes no resolver file, starts
//! no announcer, and mounts, binds, and unmounts nothing; and, with the clone's activation,
//! through an adapter of
//! the tests' own, which takes what the host sends, applies the record of the Activate that the
//! activation accepted through the fake system, and spawns an execution for each request that
//! the activation serves.
//!
//! One test runs the live system's generation of a machine ID, its write of a resolver file, and
//! their binds with the kernel itself, over a tmpfs that stands for the root, which needs the
//! privilege to mount and Linux 6.8 or later; it is ignored unless asked for:
//! `cargo test --all-features -- --ignored guest::clone::activation::apply`, as root. The host
//! export's mount, which needs a virtio-fs device, is `Root::mount_export`'s, which its own
//! tests cover.

use std::cell::Cell;
use std::error::Error as _;
use std::fs;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::num::NonZeroU64;
use std::os::unix::fs::{PermissionsExt, symlink};
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
use crate::guest::mappings::MappingError;
use crate::guest::sys::{self, Errno, Mode, MountFlags, OpenFlags};
use crate::protocol::root::{DISTRO_MMIO_BASE, SCRATCH_MMIO_BASE};
use crate::uuid::Uuid;

/// The runtime directory of the tests' template.
const RUNTIME: &str = "/run/nvx";
/// The virtio-fs tag of the host filesystem device of the tests' template.
const TAG: &str = "microvm";
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
/// The GUID that a record of the tests expects of its GPT disk.
const DISK_GUID: Uuid = Uuid::from_bytes([0xd1; 16]);
/// The most bytes that the first partition of the tests' GPT disks may hold.
const FIRST_PARTITION_BOUND: u64 = 4096;
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
/// The hostname of the tests' records.
const HOSTNAME: &[u8] = b"clone";
/// The DNS server of the tests' networks.
const DNS: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
/// The endpoint of the proxy of the tests' networks.
const PROXY: &str = "http://10.0.0.1:3128";
/// The IPv4 gateway of the tests' template's NIC.
const GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);

/// Returns whether a failure's cause is the one that a test expects.
type Expected = fn(&ActivationCause) -> bool;

/// Returns whether an account's refusal is the one that a test expects.
type Refusal = fn(&AccountError) -> bool;

/// Returns whether an application's failure is the one that a test expects.
type Failed = fn(&ApplyError) -> bool;

/// What a clone did, in order: each call that the application made of the fake system, and each
/// execution that the tests' adapter spawned.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Effect {
    /// The application asked the kernel to reread the partition table of the GPT disk on this
    /// distro device, which drops and creates the block devices of the disk's partitions.
    Reread(Device),
    /// The application assembled the root of this assembly, with a fresh upper, which mounts it.
    Assemble(Assembly),
    /// The application checked this identity's account in the root, which reads the image's
    /// account databases.
    Account(Identity),
    /// The application generated a machine ID, which it wrote to a new file at this path.
    Generate(PathBuf),
    /// The application mounted the host export, the virtio-fs device of this tag, with this
    /// access.
    MountExport(String, Access),
    /// The application wrote a resolver file that names this DNS server to a new file at this
    /// path.
    Resolver(PathBuf, IpAddr),
    /// The application started the announcer of the workload's listeners, to this gateway.
    Announce(Ipv4Addr),
    /// The announcer of the workload's listeners was stopped.
    Stop,
    /// The application bound the runtime's files of this view into the root, for every
    /// execution.
    Bind(View),
    /// The root of the host export was closed.
    Close,
    /// The runtime's binds were removed.
    Remove,
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
    /// The error with which the kernel refuses each reread of a partition table, if it does.
    refusing: Option<Errno>,
    /// The kind of the error with which each generation of a machine ID fails, if it does.
    generating: Option<io::ErrorKind>,
    /// Whether each mount of the host export fails, as when the template's filesystem device has
    /// no export attached.
    unmountable: bool,
    /// The kind of the error with which each write of a resolver file fails, if it does.
    unwritable: Option<io::ErrorKind>,
    /// The error with which each start of the announcer of the workload's listeners fails, if it
    /// does.
    unannounced: Option<fn() -> StartError>,
    /// The error with which each bind of the runtime's files fails, if it does, as when the
    /// image's `/etc/machine-id` or `/etc/resolv.conf` is a directory.
    unbindable: Option<fn() -> entry::Error>,
    /// Whether each removal of the runtime's binds fails, as when another mount covers one.
    stuck: bool,
    /// What the clone did, in order.
    effects: Vec<Effect>,
    /// The deadline of each reread and assembly, in order, by which it waits for its devices.
    deadlines: Vec<Option<Instant>>,
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
            refusing: None,
            generating: None,
            unmountable: false,
            unwritable: None,
            unannounced: None,
            unbindable: None,
            stuck: false,
            effects: Vec::new(),
            deadlines: Vec::new(),
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

    /// Returns a system whose image holds the tests' account databases, and whose kernel refuses
    /// each reread of a partition table with `error`, as while one of the disk's partitions is
    /// open.
    fn refusing(error: Errno) -> Self {
        Self {
            refusing: Some(error),
            ..Self::new()
        }
    }

    /// Returns a system whose image holds the tests' account databases, and each of whose
    /// generations of a machine ID fails with an error of `kind`.
    fn generating(kind: io::ErrorKind) -> Self {
        Self {
            generating: Some(kind),
            ..Self::new()
        }
    }

    /// Returns a system whose image holds the tests' account databases, and each of whose mounts
    /// of the host export fails.
    fn unmountable() -> Self {
        Self {
            unmountable: true,
            ..Self::new()
        }
    }

    /// Returns a system whose image holds the tests' account databases, and each of whose writes
    /// of a resolver file fails with an error of `kind`.
    fn unwritable(kind: io::ErrorKind) -> Self {
        Self {
            unwritable: Some(kind),
            ..Self::new()
        }
    }

    /// Returns a system whose image holds the tests' account databases, and each of whose starts
    /// of the announcer of the workload's listeners fails with the error that `error` returns.
    fn unannounced(error: fn() -> StartError) -> Self {
        Self {
            unannounced: Some(error),
            ..Self::new()
        }
    }

    /// Returns a system whose image holds the tests' account databases, and each of whose binds
    /// of the runtime's files fails with the error that `error` returns, once the entry checked
    /// its inputs.
    fn unbindable(error: fn() -> entry::Error) -> Self {
        Self {
            unbindable: Some(error),
            ..Self::new()
        }
    }

    /// Returns a system whose image holds the tests' account databases, and each of whose
    /// removals of the runtime's binds fails.
    fn stuck() -> Self {
        Self {
            stuck: true,
            ..Self::new()
        }
    }

    /// Returns the path of the root that each assembly returns.
    fn root(&self) -> PathBuf {
        self.image.path().to_owned()
    }

    /// Returns how many machine IDs the application generated.
    fn generated(&self) -> usize {
        let generated = |effect: &&Effect| matches!(effect, Effect::Generate(_));
        self.effects.iter().filter(generated).count()
    }
}

impl System for FakeSystem {
    type Root = PathBuf;
    type Export = ();
    type Binds = ();
    type Announcer = Ipv4Addr;

    fn reread(
        &mut self,
        distro: Device,
        _timeout: Duration,
        deadline: Option<Instant>,
    ) -> Result<(), root::Error> {
        self.effects.push(Effect::Reread(distro));
        self.deadlines.push(deadline);
        match self.refusing {
            Some(error) => Err(root::Error::Device {
                role: root::Role::Distro,
                step: root::DeviceStep::Reread,
                device: PathBuf::from("/dev/vda"),
                error: error.into(),
            }),
            None => Ok(()),
        }
    }

    fn assemble(
        &mut self,
        assembly: &Assembly,
        deadline: Option<Instant>,
    ) -> Result<PathBuf, AssemblyError> {
        self.effects.push(Effect::Assemble(assembly.clone()));
        self.deadlines.push(deadline);
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

    fn generate(&mut self, path: &Path) -> io::Result<MachineId> {
        self.effects.push(Effect::Generate(path.to_owned()));
        match self.generating {
            Some(kind) => Err(kind.into()),
            None => Ok(MachineId::Generated(path.to_owned())),
        }
    }

    fn mount_export(
        &mut self,
        root: &mut PathBuf,
        tag: &str,
        access: Access,
    ) -> Result<(), root::Error> {
        assert_eq!(*root, self.root());
        self.effects
            .push(Effect::MountExport(tag.to_owned(), access));
        if self.unmountable {
            // The virtio-fs device has no export attached.
            return Err(root::Error::Mount {
                mount: root::Mount::Export {
                    tag: tag.to_owned(),
                },
                error: Errno::ENOENT.into(),
            });
        }
        Ok(())
    }

    fn close(&mut self, (): ()) {
        self.effects.push(Effect::Close);
    }

    fn write_resolver(&mut self, path: &Path, server: IpAddr) -> io::Result<()> {
        self.effects.push(Effect::Resolver(path.to_owned(), server));
        match self.unwritable {
            Some(kind) => Err(kind.into()),
            None => Ok(()),
        }
    }

    fn announce(&mut self, gateway: Ipv4Addr) -> Result<Ipv4Addr, StartError> {
        self.effects.push(Effect::Announce(gateway));
        match self.unannounced {
            Some(error) => Err(error()),
            None => Ok(gateway),
        }
    }

    fn stop(&mut self, announcer: Ipv4Addr) {
        assert_eq!(announcer, GATEWAY);
        self.effects.push(Effect::Stop);
    }

    fn bind(&mut self, root: &PathBuf, view: &View) -> Result<(), entry::Error> {
        assert_eq!(*root, self.root());
        self.effects.push(Effect::Bind(view.clone()));
        // The entry checks its inputs, the hostname among them, before it binds anything.
        view.entry(root).check()?;
        match self.unbindable {
            Some(error) => Err(error()),
            None => Ok(()),
        }
    }

    fn remove(&mut self, (): ()) -> Result<(), mappings::TeardownError> {
        self.effects.push(Effect::Remove);
        if self.stuck {
            return Err(mappings::TeardownError::of(
                "/etc/machine-id",
                MappingError::MountPoint,
            ));
        }
        Ok(())
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
    /// What the application activated: the root, the export's root, if any, the runtime's binds
    /// on the root, the identity in which every execution runs, if any, the workload's hostname
    /// and machine ID, the resolver file, if any, the proxy's endpoint, if any, and the gateway of
    /// the announcer of the workload's listeners, if any.
    applied: Option<Activated<PathBuf, (), (), Ipv4Addr>>,
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
                let deadline = self.activation.deadline();
                let applied = apply_with(&mut self.system, &record, &template(), deadline);
                let result = match applied {
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
/// the sizes that they must have, its timeout, its host filesystem device's tag, and its NIC's
/// gateway.
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
        filesystem_tag: TAG.to_owned(),
        gateway: Some(GATEWAY),
    }
}

/// Returns an EROFS image, with the UUID `uuid` that the record expects of it, if any.
fn erofs(uuid: Option<Uuid>) -> activation::Image {
    activation::Image::Erofs {
        binding: activation::Identity::from_bytes([0x11; activation::Identity::LEN]),
        uuid,
    }
}

/// Returns a GPT disk, with the GUID `disk_guid` that the record expects of it, if any, whose
/// first partition holds at most [`FIRST_PARTITION_BOUND`] bytes.
fn gpt(disk_guid: Option<Uuid>) -> activation::Image {
    activation::Image::Gpt {
        binding: activation::Identity::from_bytes([0x11; activation::Identity::LEN]),
        disk_guid,
        first_partition_bound: FIRST_PARTITION_BOUND,
    }
}

/// Returns the image of the root's assembly of a GPT disk of [`gpt`]`(disk_guid)`.
fn gpt_image(disk_guid: Option<Uuid>) -> root::Image {
    let expected = crate::gpt::Expected::new();
    root::Image::Gpt {
        expected: match disk_guid {
            Some(guid) => expected.disk_guid(guid),
            None => expected,
        },
        first_partition_bound: FIRST_PARTITION_BOUND as usize,
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
            hostname: HOSTNAME.to_vec(),
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

/// Returns the record of the tests' Activate, as [`accepted`] returns it, but whose workload sees
/// a machine ID that the guest generates.
fn with_generated_id() -> ActivationRecord {
    let mut configuration = accepted().configuration().clone();
    configuration.workload.machine_id = MachineIdMode::Generated;
    clone_record(configuration)
}

/// Returns an export of `kind`, of the tests' plan.
fn export(kind: ExportKind) -> Export {
    Export {
        kind,
        plan: activation::Identity::from_bytes([0x77; activation::Identity::LEN]),
    }
}

/// Returns a network whose resolver names `dns`, if any, and whose proxy is `proxy`, if any,
/// with published listeners.
fn network(dns: Option<IpAddr>, proxy: Option<&str>) -> Network {
    Network {
        policy: activation::Identity::from_bytes([0x88; activation::Identity::LEN]),
        dns,
        publication: Publication::Published,
        proxy: proxy.map(str::to_owned),
    }
}

/// Returns a network as [`network`] does, but without published listeners.
fn unpublished(dns: Option<IpAddr>, proxy: Option<&str>) -> Network {
    Network {
        publication: Publication::Unpublished,
        ..network(dns, proxy)
    }
}

/// Returns the record of the tests' Activate, as [`with_generated_id`] returns it, but with an
/// aggregate export, of three mappings, and a network that names [`DNS`] and [`PROXY`], and
/// publishes the workload's listeners.
fn with_everything() -> ActivationRecord {
    let mut configuration = with_generated_id().configuration().clone();
    configuration.export = Some(export(ExportKind::Aggregate));
    configuration.mappings = 3;
    configuration.network = Some(network(Some(DNS), Some(PROXY)));
    clone_record(configuration)
}

/// Returns where the application writes the machine ID that it generates with the tests'
/// template.
fn machine_id_path() -> PathBuf {
    Path::new(RUNTIME).join(MACHINE_ID_FILE)
}

/// Returns the machine ID that the application generates with the tests' template.
fn generated_id() -> MachineId {
    MachineId::Generated(machine_id_path())
}

/// Returns where the application writes the resolver file with the tests' template.
fn resolver_path() -> PathBuf {
    Path::new(RUNTIME).join(RESOLVER_FILE)
}

/// Returns the view that the application binds for a record of the tests' hostname, whose
/// workload runs as `identity`, if any, and sees `machine_id`, without a resolver file.
fn view(identity: Option<Identity>, machine_id: MachineId) -> View {
    View {
        identity: identity.unwrap_or_else(|| Identity::new(0, 0)),
        hostname: HOSTNAME.to_vec(),
        machine_id,
        resolver: None,
    }
}

/// Returns the view that the application binds for [`with_everything`]'s record: of its
/// workload identity, with the generated machine ID and the resolver file.
fn full_view() -> View {
    View {
        resolver: Some(resolver_path()),
        ..view(Some(workload()), generated_id())
    }
}

/// Returns what the application activates through `system` for a record of the tests' hostname,
/// whose workload runs as `identity`, if any, and sees `machine_id`, without an export or a
/// network.
fn activated(
    system: &FakeSystem,
    identity: Option<Identity>,
    machine_id: MachineId,
) -> Activated<PathBuf, (), (), Ipv4Addr> {
    Activated {
        root: system.root(),
        export: None,
        binds: (),
        identity,
        hostname: HOSTNAME.to_vec(),
        machine_id,
        resolver: None,
        proxy: None,
        announcer: None,
    }
}

/// Returns the effects of the application of [`with_everything`]'s record, which the system
/// records until it bound the runtime's files.
fn applied_everything() -> Vec<Effect> {
    vec![
        Effect::Assemble(accepted_assembly()),
        Effect::Account(workload()),
        Effect::Generate(machine_id_path()),
        Effect::MountExport(TAG.to_owned(), Access::ReadWrite),
        Effect::Resolver(resolver_path(), DNS),
        Effect::Announce(GATEWAY),
        Effect::Bind(full_view()),
    ]
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
    // full: none starts a process but the announcer of the workload's listeners, and the adapter
    // spawns each execution itself, once the activation serves its request. Every record asks
    // for a generated machine ID, has an export, names a DNS server, and publishes the workload's
    // listeners, which only the application of an accepted record generates, mounts, writes, and
    // announces.
    let base = Instant::now();
    let record = with_everything().encode();
    let settings = with_everything().configuration().clone();
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
        image: gpt(None),
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

    // A GPT disk, which the activation accepted, whose partition table the kernel refuses to
    // reread, fails the application before anything is mounted: the reread is its one call.
    let mut adapter = Adapter::restricted(FakeSystem::refusing(Errno::EBUSY), base);
    let failure = adapter
        .take(base, ActivationInput::Activate(&disk))
        .unwrap_err();
    assert!(
        matches!(
            apply_error(&failure),
            ApplyError::Gpt(root::Error::Device {
                step: root::DeviceStep::Reread,
                ..
            })
        ),
        "{failure}"
    );
    assert_eq!(failure.state(), ActivationState::Activating);
    assert_eq!(adapter.activation.state(), ActivationState::Failed);
    assert_eq!(adapter.system.effects, [Effect::Reread(template().distro)]);
    assert_eq!(adapter.applied, None);
    assert_eq!(adapter.ready, None);
    assert!(!adapter.fence.is_open());

    // Past the deadline, even a valid Activate does nothing.
    let mut adapter = Adapter::restricted(FakeSystem::new(), base);
    let failure = adapter
        .take(base + TIMEOUT, ActivationInput::Activate(&record))
        .unwrap_err();
    adapter.assert_untouched(&failure, |cause| matches!(cause, ActivationCause::Expired));

    // Only a valid Activate mounts the root, reads its account databases, generates the machine
    // ID, mounts the export, writes the resolver file, starts the announcer, and binds the
    // runtime's files, before Ready, and an execution spawns only once Ready opened the fence.
    let mut adapter = Adapter::restricted(FakeSystem::new(), base);
    adapter.take(base, ActivationInput::Flow).unwrap();
    assert!(adapter.system.effects.is_empty());
    adapter
        .take(base, ActivationInput::Activate(&record))
        .unwrap();
    assert_eq!(adapter.system.effects, applied_everything());
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
        Some(activated(
            &adapter.system,
            Some(workload()),
            MachineId::Image
        ))
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
            Effect::Bind(view(Some(workload()), MachineId::Image)),
            Effect::Spawn,
            Effect::Spawn,
        ]
    );
    // The assembly waited for the devices until the activation's deadline at the latest.
    assert_eq!(adapter.system.deadlines, [Some(base + TIMEOUT)]);
}

#[test]
fn assembly_and_identity_inputs_are_taken_exactly_from_the_record() {
    let template = template();
    // Each image, with what the assembly expects of it, and whether the kernel rereads its
    // partition table first.
    let images = [
        (erofs(None), root::Image::Erofs { uuid: None }, false),
        (
            erofs(Some(IMAGE_UUID)),
            root::Image::Erofs {
                uuid: Some(IMAGE_UUID),
            },
            false,
        ),
        (gpt(None), gpt_image(None), true),
        (gpt(Some(DISK_GUID)), gpt_image(Some(DISK_GUID)), true),
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
    for (image, root_image, reread) in images {
        for (upper, root_upper) in uppers {
            for (overlay, root_overlay) in overlays {
                for identity in [None, Some(WORKLOAD)] {
                    let record = clone_record(configuration(image, upper, overlay, identity));
                    let mut system = FakeSystem::new();
                    let applied = apply_with(&mut system, &record, &template, None).unwrap();
                    let assembly = Assembly {
                        runtime: template.runtime.clone(),
                        distro: template.distro,
                        image: root_image.clone(),
                        upper: root_upper,
                        overlay: root_overlay,
                        timeout: template.timeout,
                    };
                    let identity = identity.map(|_| workload());
                    let mut effects = Vec::new();
                    if reread {
                        effects.push(Effect::Reread(template.distro));
                    }
                    effects.push(Effect::Assemble(assembly));
                    effects.extend(identity.clone().map(Effect::Account));
                    effects.push(Effect::Bind(view(identity.clone(), MachineId::Image)));
                    assert_eq!(system.effects, effects, "{record:?}");
                    assert_eq!(applied, activated(&system, identity, MachineId::Image));
                }
            }
        }
    }

    // The record's other settings, which the application leaves alone, change no input: its
    // bindings, its export's shape and plan, the number of its mappings, which the adapter's
    // order takes, its network's policy, its resources, and its restore section.
    let mut other = with_everything().configuration().clone();
    other.image = activation::Image::Erofs {
        binding: activation::Identity::from_bytes([0x66; activation::Identity::LEN]),
        uuid: Some(IMAGE_UUID),
    };
    other.export = Some(Export {
        kind: ExportKind::Directory,
        plan: activation::Identity::from_bytes([0x99; activation::Identity::LEN]),
    });
    other.mappings = 7;
    other.network = Some(Network {
        policy: activation::Identity::from_bytes([0xaa; activation::Identity::LEN]),
        ..network(Some(DNS), Some(PROXY))
    });
    other.resources = ExpectedResources {
        cpus: 8,
        memory: 1 << 33,
    };
    let other = ActivationRecord::new(None, other).unwrap();
    let (mut first, mut second) = (FakeSystem::new(), FakeSystem::new());
    let first_applied = apply_with(&mut first, &with_everything(), &template, None).unwrap();
    let second_applied = apply_with(&mut second, &other, &template, None).unwrap();
    assert_eq!(first.effects, applied_everything());
    assert_eq!(first.effects, second.effects);
    assert_eq!(
        first_applied,
        Activated {
            root: first.root(),
            ..second_applied
        }
    );
}

#[test]
fn a_gpt_disk_s_partition_table_is_reread_before_its_root_is_assembled() {
    // The kernel rereads the table from the clone's disk before the assembly reads and checks
    // it, as at a cold boot, where it read the table when it found the disk; the clone then
    // activates as with an EROFS image, over a fresh upper.
    let base = Instant::now();
    let record = clone_record(configuration(
        gpt(Some(DISK_GUID)),
        scratch(activation::Journal::Forbidden),
        activation::OverlayMode::UntrustedLowers,
        Some(WORKLOAD),
    ));
    let mut adapter = Adapter::restricted(FakeSystem::new(), base);
    adapter
        .take(base, ActivationInput::Activate(&record.encode()))
        .unwrap();
    let template = template();
    let assembly = Assembly {
        runtime: template.runtime.clone(),
        distro: template.distro,
        image: gpt_image(Some(DISK_GUID)),
        upper: root::Upper::Scratch {
            device: template.scratch,
            journal: root::Journal::Forbidden,
        },
        overlay: root::OverlayMode::UntrustedLowers,
        timeout: template.timeout,
    };
    assert_eq!(
        adapter.system.effects,
        [
            Effect::Reread(template.distro),
            Effect::Assemble(assembly),
            Effect::Account(workload()),
            Effect::Bind(view(Some(workload()), MachineId::Image)),
        ]
    );
    // The reread waited for the distro device, and the assembly for each device, until the
    // activation's deadline at the latest.
    assert_eq!(adapter.system.deadlines, [Some(base + TIMEOUT); 2]);
    assert_eq!(adapter.activation.state(), ActivationState::Ready);
    assert_eq!(adapter.ready, Some(ReadyActivation::of(&record)));
    assert!(adapter.fence.is_open());
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
    // own identity, which the adapter's entry checks. The entry that binds the runtime's files
    // binds the same files whatever its identity.
    let anyone = clone_record(configuration(
        erofs(Some(IMAGE_UUID)),
        activation::Upper::Tmpfs,
        activation::OverlayMode::UntrustedLowers,
        None,
    ));
    let mut system = FakeSystem::with(b"", b"");
    let applied = apply_with(&mut system, &anyone, &template(), None).unwrap();
    assert_eq!(applied.identity, None);
    assert_eq!(
        system.effects,
        [
            Effect::Assemble(accepted_assembly()),
            Effect::Bind(view(None, MachineId::Image)),
        ]
    );
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
fn a_generated_machine_id_is_generated_once_after_the_gate_for_every_execution() {
    // The application generates the machine ID once the activation accepted the record, after
    // the root's assembly and the account's check, in the template's runtime directory, and
    // binds it for every execution, through the entry of the record's hostname.
    let base = Instant::now();
    let record = with_generated_id();
    let bytes = record.encode();
    let mut adapter = Adapter::restricted(FakeSystem::new(), base);
    adapter
        .take(base, ActivationInput::Activate(&bytes))
        .unwrap();
    let expected = [
        Effect::Assemble(accepted_assembly()),
        Effect::Account(workload()),
        Effect::Generate(machine_id_path()),
        Effect::Bind(view(Some(workload()), generated_id())),
    ];
    assert_eq!(adapter.system.effects, expected);
    assert_eq!(
        adapter.applied,
        Some(activated(&adapter.system, Some(workload()), generated_id()))
    );
    assert_eq!(adapter.ready, Some(ReadyActivation::of(&record)));
    assert_eq!(machine_id_path(), Path::new("/run/nvx/workload-machine-id"));

    // Every execution, a retry of the Activate, and a retry in a later session reuse it: nothing
    // generates or binds it again.
    adapter.take(base, ActivationInput::Request).unwrap();
    adapter
        .take(base, ActivationInput::Activate(&bytes))
        .unwrap();
    adapter.take(base, ActivationInput::Reset).unwrap();
    adapter.ready = None;
    adapter
        .take(base, ActivationInput::Activate(&bytes))
        .unwrap();
    assert_eq!(adapter.ready, Some(ReadyActivation::of(&record)));
    adapter.take(base, ActivationInput::Request).unwrap();
    assert_eq!(adapter.system.generated(), 1);
    assert_eq!(
        adapter.system.effects[expected.len()..],
        [Effect::Spawn, Effect::Spawn]
    );

    // The image's own machine ID generates nothing: its view, which the entry binds nothing
    // for, leaves the image's `/etc/machine-id` alone.
    let mut system = FakeSystem::new();
    let applied = apply_with(&mut system, &accepted(), &template(), None).unwrap();
    assert_eq!(system.generated(), 0);
    assert_eq!(
        applied,
        activated(&system, Some(workload()), MachineId::Image)
    );
}

#[test]
fn the_view_s_entry_takes_the_record_s_hostname_and_machine_id() {
    // The entry through which the application binds the runtime's files checks the record's
    // hostname, which every execution's entry takes, and refuses one that no execution could
    // set.
    let view_entry = |hostname: &[u8]| {
        let named = View {
            hostname: hostname.to_vec(),
            ..view(None, MachineId::Image)
        };
        named.entry(Path::new(RUNTIME))
    };
    view_entry(HOSTNAME).check().unwrap();
    for refused in [&b""[..], &[b'h'; 65], b"a\0b"] {
        let refusal = view_entry(refused).check();
        assert!(
            matches!(refusal, Err(entry::Error::Hostname)),
            "{refused:?}: {refusal:?}"
        );
    }

    // It binds a generated machine ID, a resolver file, or both, and nothing for the image's own
    // machine ID without a resolver file: only the former open the root, which a temporary
    // directory, not the root of a mount, fails.
    let directory = tempfile::tempdir().unwrap();
    let image = view(None, MachineId::Image).entry(directory.path());
    image.bind_runtime().unwrap().remove().unwrap();
    let resolver = View {
        resolver: Some(resolver_path()),
        ..view(None, MachineId::Image)
    };
    for bound in [view(None, generated_id()), resolver, full_view()] {
        let error = bound.entry(directory.path()).bind_runtime().unwrap_err();
        assert!(matches!(error, entry::Error::Root(_)), "{bound:?}: {error}");
    }
}

#[test]
fn a_machine_id_or_a_view_that_fails_fails_the_activation_once_the_root_is_torn_down() {
    let base = Instant::now();
    let bytes = with_generated_id().encode();
    // A file at the machine ID's path already, as once a boot generated its ID, and a bind that
    // the image refuses, as one whose `/etc/machine-id` is a directory.
    let generation = Effect::Generate(machine_id_path());
    let bind = Effect::Bind(view(Some(workload()), generated_id()));
    let cases = [
        (
            FakeSystem::generating(io::ErrorKind::AlreadyExists),
            vec![generation.clone()],
        ),
        (
            FakeSystem::unbindable(|| entry::Error::MachineId(MappingError::TargetKind)),
            vec![generation, bind],
        ),
    ];
    for (system, effects) in cases {
        let mut adapter = Adapter::restricted(system, base);
        let failure = adapter
            .take(base, ActivationInput::Activate(&bytes))
            .unwrap_err();
        assert_eq!(failure.state(), ActivationState::Activating);
        match apply_error(&failure) {
            ApplyError::MachineId {
                error,
                teardown: None,
            } => assert_eq!(error.kind(), io::ErrorKind::AlreadyExists),
            ApplyError::View {
                error: entry::Error::MachineId(MappingError::TargetKind),
                teardown: None,
            } => {}
            error => panic!("{error}"),
        }
        // The root was assembled, its account checked, and the root torn down, without a Ready,
        // and behind the closed fence.
        let mut expected = vec![
            Effect::Assemble(accepted_assembly()),
            Effect::Account(workload()),
        ];
        expected.extend(effects);
        expected.push(Effect::Teardown);
        assert_eq!(adapter.system.effects, expected);
        assert_eq!(adapter.activation.state(), ActivationState::Failed);
        assert_eq!(adapter.applied, None);
        assert_eq!(adapter.ready, None);
        assert!(!adapter.fence.is_open());
        // The failure is final: no retry applies the record again.
        assert!(
            adapter
                .take(base, ActivationInput::Activate(&bytes))
                .is_err()
        );
        assert_eq!(adapter.system.effects, expected);
    }
}

#[test]
fn the_runtime_s_binds_are_removed_before_the_root_is_torn_down() {
    // The binds keep the root's mount busy, so they go first, and the export's root, which keeps
    // the export's mount busy, before them, unless the adapter took it, and the announcer before
    // anything.
    let mut system = FakeSystem::new();
    let root = system.root();
    teardown_with(&mut system, None, None, (), root).unwrap();
    assert_eq!(system.effects, [Effect::Remove, Effect::Teardown]);
    let mut system = FakeSystem::new();
    let root = system.root();
    teardown_with(&mut system, None, Some(()), (), root).unwrap();
    assert_eq!(
        system.effects,
        [Effect::Close, Effect::Remove, Effect::Teardown]
    );
    let mut system = FakeSystem::new();
    let root = system.root();
    teardown_with(&mut system, Some(GATEWAY), Some(()), (), root).unwrap();
    assert_eq!(
        system.effects,
        [
            Effect::Stop,
            Effect::Close,
            Effect::Remove,
            Effect::Teardown
        ]
    );

    // A removal that fails leaves those binds mounted, and the teardown is attempted all the
    // same; the error reports both.
    let mut system = FakeSystem::stuck();
    let root = system.root();
    let error = teardown_with(&mut system, None, Some(()), (), root).unwrap_err();
    assert_eq!(
        system.effects,
        [Effect::Close, Effect::Remove, Effect::Teardown]
    );
    assert!(error.root().is_none());
    let binds = error.binds().unwrap();
    assert_eq!(binds.failures().len(), 1);
    assert_eq!(binds.failures()[0].target(), Path::new("/etc/machine-id"));
    assert_eq!(error.to_string(), binds.to_string());
    assert!(
        error
            .to_string()
            .starts_with("failed to unmount the mapping at /etc/machine-id: "),
        "{error}"
    );
}

#[test]
fn an_export_is_mounted_through_the_template_s_device_and_handed_to_the_adapter() {
    // A record's export, of either shape, is mounted once, before Ready, read-write, through the
    // template's filesystem device, once the workload's identity was checked, and the adapter
    // gets its root, to map the host's mapping table from.
    let base = Instant::now();
    for kind in [ExportKind::Directory, ExportKind::Aggregate] {
        let mut configuration = accepted().configuration().clone();
        configuration.export = Some(export(kind));
        configuration.mappings = 2;
        let record = clone_record(configuration).encode();
        let mut adapter = Adapter::restricted(FakeSystem::new(), base);
        adapter
            .take(base, ActivationInput::Activate(&record))
            .unwrap();
        let expected = [
            Effect::Assemble(accepted_assembly()),
            Effect::Account(workload()),
            Effect::MountExport(TAG.to_owned(), Access::ReadWrite),
            Effect::Bind(view(Some(workload()), MachineId::Image)),
        ];
        assert_eq!(adapter.system.effects, expected, "{kind:?}");
        assert_eq!(
            adapter.applied,
            Some(Activated {
                export: Some(()),
                ..activated(&adapter.system, Some(workload()), MachineId::Image)
            })
        );
        // A retry and an execution mount nothing again.
        adapter
            .take(base, ActivationInput::Activate(&record))
            .unwrap();
        adapter.take(base, ActivationInput::Request).unwrap();
        assert_eq!(adapter.system.effects[expected.len()..], [Effect::Spawn]);
    }

    // A record without an export mounts none.
    let mut system = FakeSystem::new();
    let applied = apply_with(&mut system, &accepted(), &template(), None).unwrap();
    assert_eq!(applied.export, None);
    assert!(
        !system
            .effects
            .iter()
            .any(|effect| matches!(effect, Effect::MountExport(..))),
        "{:?}",
        system.effects
    );
}

#[test]
fn a_network_s_dns_server_is_written_to_a_resolver_file_that_every_execution_sees() {
    // A network that names a DNS server, IPv4 or IPv6, has the application write a resolver file
    // that names it in the template's runtime directory, and bind it with the runtime's other
    // files, in the one bind; the network's proxy endpoint goes back to the adapter.
    let ipv6 = IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0xa00, 1));
    for dns in [DNS, ipv6] {
        let mut configuration = accepted().configuration().clone();
        configuration.network = Some(unpublished(Some(dns), Some(PROXY)));
        let record = clone_record(configuration);
        let mut system = FakeSystem::new();
        let applied = apply_with(&mut system, &record, &template(), None).unwrap();
        let bound = View {
            resolver: Some(resolver_path()),
            ..view(Some(workload()), MachineId::Image)
        };
        let expected = [
            Effect::Assemble(accepted_assembly()),
            Effect::Account(workload()),
            Effect::Resolver(resolver_path(), dns),
            Effect::Bind(bound),
        ];
        assert_eq!(system.effects, expected, "{dns}");
        assert_eq!(
            applied,
            Activated {
                resolver: Some(resolver_path()),
                proxy: Some(PROXY.to_owned()),
                ..activated(&system, Some(workload()), MachineId::Image)
            }
        );
    }

    // A network without a DNS server, whether or not it names a proxy, writes no resolver file,
    // so that every execution sees the image's own, and the image's own machine ID binds
    // nothing.
    for proxy in [None, Some(PROXY)] {
        let mut configuration = accepted().configuration().clone();
        configuration.network = Some(unpublished(None, proxy));
        let record = clone_record(configuration);
        let mut system = FakeSystem::new();
        let applied = apply_with(&mut system, &record, &template(), None).unwrap();
        let expected = [
            Effect::Assemble(accepted_assembly()),
            Effect::Account(workload()),
            Effect::Bind(view(Some(workload()), MachineId::Image)),
        ];
        assert_eq!(system.effects, expected, "{proxy:?}");
        assert_eq!(
            applied,
            Activated {
                proxy: proxy.map(str::to_owned),
                ..activated(&system, Some(workload()), MachineId::Image)
            }
        );
    }
}

#[test]
fn a_resolver_file_names_its_dns_server_once_and_every_execution_may_read_it() {
    // The file names the server in its canonical text, as NVX's init writes the initramfs's,
    // with mode 0444, whatever the umask.
    let directory = tempfile::tempdir().unwrap();
    let ipv6 = IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0xa00, 1));
    let cases = [
        ("v4", DNS, &b"nameserver 10.0.0.1\n"[..]),
        ("v6", ipv6, &b"nameserver fd00::a00:1\n"[..]),
    ];
    for (name, server, text) in cases {
        let path = directory.path().join(name);
        sys::with_thread_umask(0o077, || write_resolver_file(&path, server)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), text);
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o7777, 0o444, "{mode:o}");
    }

    // A file there already fails the write, and is left alone, and so is the file that a
    // symbolic link there names, or the path where a dangling one leads.
    let written = directory.path().join("v4");
    let error = write_resolver_file(&written, ipv6).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    let link = directory.path().join("link");
    symlink(&written, &link).unwrap();
    let error = write_resolver_file(&link, ipv6).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read(&written).unwrap(), b"nameserver 10.0.0.1\n");
    let absent = directory.path().join("absent");
    let dangling = directory.path().join("dangling");
    symlink(&absent, &dangling).unwrap();
    let error = write_resolver_file(&dangling, DNS).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert!(!absent.exists());
}

#[test]
fn an_export_or_a_network_that_fails_fails_the_activation_once_the_root_is_torn_down() {
    // The export's mount fails with the export's class, and the resolver file's write and bind,
    // and the announcer's start, with the network's, while the machine ID's bind fails with the
    // view's, each once the application stopped the announcer, if it started it, closed the
    // export's root, if it mounted the export, and tore the root down, which unmounts the export.
    let base = Instant::now();
    let bytes = with_everything().encode();
    let failed = |done: usize, stopped: bool, closed: bool| {
        let mut effects = applied_everything()[..done].to_vec();
        if stopped {
            effects.push(Effect::Stop);
        }
        if closed {
            effects.push(Effect::Close);
        }
        effects.push(Effect::Teardown);
        effects
    };
    let cases: [(FakeSystem, Vec<Effect>, Failed); 6] = [
        (
            FakeSystem::unmountable(),
            failed(4, false, false),
            |error| {
                matches!(
                    error,
                    ApplyError::Export {
                        error: root::Error::Mount {
                            mount: root::Mount::Export { tag },
                            ..
                        },
                        teardown: None,
                    } if tag == TAG
                )
            },
        ),
        (
            FakeSystem::unwritable(io::ErrorKind::AlreadyExists),
            failed(5, false, true),
            |error| {
                matches!(
                    error,
                    ApplyError::Network {
                        error: NetworkError::Write(error),
                        teardown: None,
                    } if error.kind() == io::ErrorKind::AlreadyExists
                )
            },
        ),
        (
            FakeSystem::unannounced(|| StartError::NoPrivilegedPort),
            failed(6, false, true),
            |error| {
                matches!(
                    error,
                    ApplyError::Network {
                        error: NetworkError::Announcer(StartError::NoPrivilegedPort),
                        teardown: None,
                    }
                )
            },
        ),
        (
            FakeSystem::unbindable(|| entry::Error::Resolver(MappingError::TargetKind)),
            failed(7, true, true),
            |error| {
                matches!(
                    error,
                    ApplyError::Network {
                        error: NetworkError::Bind(entry::Error::Resolver(MappingError::TargetKind)),
                        teardown: None,
                    }
                )
            },
        ),
        // A resolver's bind that failed once the machine ID's bind stayed mounted.
        (
            FakeSystem::unbindable(|| entry::Error::Rollback {
                error: Box::new(entry::Error::Resolver(MappingError::TargetKind)),
                teardown: mappings::TeardownError::of("/etc/machine-id", MappingError::MountPoint),
            }),
            failed(7, true, true),
            |error| {
                matches!(
                    error,
                    ApplyError::Network {
                        error: NetworkError::Bind(entry::Error::Rollback { .. }),
                        teardown: None,
                    }
                )
            },
        ),
        (
            FakeSystem::unbindable(|| entry::Error::MachineId(MappingError::TargetKind)),
            failed(7, true, true),
            |error| {
                matches!(
                    error,
                    ApplyError::View {
                        error: entry::Error::MachineId(MappingError::TargetKind),
                        teardown: None,
                    }
                )
            },
        ),
    ];
    for (system, effects, expected) in cases {
        let mut adapter = Adapter::restricted(system, base);
        let failure = adapter
            .take(base, ActivationInput::Activate(&bytes))
            .unwrap_err();
        let error = apply_error(&failure);
        assert!(expected(error), "{error}");
        assert_eq!(adapter.system.effects, effects, "{error}");
        assert_eq!(adapter.activation.state(), ActivationState::Failed);
        assert_eq!(adapter.applied, None);
        assert_eq!(adapter.ready, None);
        assert!(!adapter.fence.is_open());
    }
}

#[test]
fn a_network_that_publishes_the_workload_s_listeners_starts_their_announcer_to_its_gateway() {
    // A network that publishes the workload's listeners has the application start their
    // announcer, to the template's gateway, once it wrote the resolver file, and before it binds
    // the runtime's files, and hand it to the adapter, which watches it; a network that publishes
    // none, and a clone without a network, start none.
    let mut configuration = accepted().configuration().clone();
    configuration.network = Some(network(None, Some(PROXY)));
    let published = clone_record(configuration.clone());
    let mut system = FakeSystem::new();
    let applied = apply_with(&mut system, &published, &template(), None).unwrap();
    let bound = Effect::Bind(view(Some(workload()), MachineId::Image));
    let assembled = [
        Effect::Assemble(accepted_assembly()),
        Effect::Account(workload()),
    ];
    assert_eq!(
        system.effects,
        [&assembled[..], &[Effect::Announce(GATEWAY), bound.clone()]].concat()
    );
    assert_eq!(
        applied,
        Activated {
            proxy: Some(PROXY.to_owned()),
            announcer: Some(GATEWAY),
            ..activated(&system, Some(workload()), MachineId::Image)
        }
    );
    configuration.network = Some(unpublished(None, Some(PROXY)));
    for record in [clone_record(configuration), accepted()] {
        let mut system = FakeSystem::new();
        let applied = apply_with(&mut system, &record, &template(), None).unwrap();
        assert_eq!(
            system.effects,
            [&assembled[..], std::slice::from_ref(&bound)].concat()
        );
        assert_eq!(applied.announcer, None);
    }

    // A template without a gateway, as one without a NIC, cannot announce: the record that
    // publishes listeners fails with the network's class, once the root was torn down, and one
    // that publishes none applies.
    let gatewayless = RootTemplate {
        gateway: None,
        ..template()
    };
    let mut system = FakeSystem::new();
    let error = apply_with(&mut system, &published, &gatewayless, None).unwrap_err();
    assert!(
        matches!(
            error,
            ApplyError::Network {
                error: NetworkError::Gateway,
                teardown: None,
            }
        ),
        "{error}"
    );
    assert_eq!(
        system.effects,
        [&assembled[..], &[Effect::Teardown]].concat()
    );
    let mut system = FakeSystem::new();
    let applied = apply_with(&mut system, &accepted(), &gatewayless, None).unwrap();
    assert_eq!(applied.announcer, None);
}

/// A tmpfs that stands for a clone's root, mounted at `root` below a temporary directory, in a
/// private mount namespace of the calling thread, beside a directory that stands for the
/// template's runtime directory, `runtime`.
struct LiveRoot {
    /// The temporary directory, which the drop removes once it detached the tmpfs.
    directory: Option<tempfile::TempDir>,
    /// The tmpfs's mount point.
    root: PathBuf,
    /// The runtime directory.
    runtime: PathBuf,
}

impl LiveRoot {
    /// Returns a root whose `/etc/machine-id` holds [`IMAGE_MACHINE_ID`], and whose
    /// `/etc/resolv.conf` holds [`IMAGE_RESOLVER`], or `None` where the test cannot run: without
    /// root, or without unique mount IDs, which Linux 6.8 added.
    fn new() -> Option<Self> {
        if !nix::unistd::geteuid().is_root() {
            report("skipped: the test needs root");
            return None;
        }
        sys::enter_private_mount_namespace().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().canonicalize().unwrap();
        let live = Self {
            directory: Some(directory),
            root: base.join("root"),
            runtime: base.join("runtime"),
        };
        fs::create_dir(&live.root).unwrap();
        fs::create_dir(&live.runtime).unwrap();
        let root = sys::path_c_string(&live.root).unwrap();
        let tmpfs = Some(c"tmpfs");
        let options = Some(c"mode=0755");
        sys::mount(tmpfs, &root, tmpfs, MountFlags::empty(), options).unwrap();
        fs::create_dir(live.root.join("etc")).unwrap();
        fs::write(live.root.join("etc/machine-id"), IMAGE_MACHINE_ID).unwrap();
        fs::write(live.root.join("etc/resolv.conf"), IMAGE_RESOLVER).unwrap();
        let flags = OpenFlags::O_PATH | OpenFlags::O_CLOEXEC;
        match sys::file_status(&sys::open(&root, flags, Mode::empty()).unwrap()) {
            Err(Errno::EOPNOTSUPP) => {
                report("skipped: the kernel has no unique mount IDs");
                None
            }
            status => {
                assert!(status.unwrap().mount_root);
                Some(live)
            }
        }
    }
}

impl Drop for LiveRoot {
    /// Detaches the tmpfs, with what is mounted below it, then removes the directory.
    fn drop(&mut self) {
        let _ = nix::mount::umount2(&self.root, nix::mount::MntFlags::MNT_DETACH);
        drop(self.directory.take());
    }
}

/// The machine ID of the image of the privileged test's root.
const IMAGE_MACHINE_ID: &[u8] = b"0123456789abcdef0123456789abcdef\n";

/// The resolver file of the image of the privileged test's root.
const IMAGE_RESOLVER: &[u8] = b"nameserver 192.0.2.53\n";

/// Writes `message` to the standard error, past the test harness's capture, as the guest's
/// programs write theirs.
fn report(message: &str) {
    use std::io::Write;

    let _ = writeln!(io::stderr(), "{message}");
}

#[test]
#[ignore = "needs root and Linux 6.8 or later"]
fn a_generated_machine_id_and_a_resolver_file_keep_the_root_busy_until_their_binds_are_removed() {
    // With the kernel itself: the live system generates the machine ID in the runtime directory
    // once, writes the resolver file there once, and binds both read-only over the root's
    // `/etc/machine-id` and `/etc/resolv.conf` through the view's entry, in the calling thread's
    // mount namespace, of which an execution's starts as a copy.
    let Some(live) = LiveRoot::new() else {
        return;
    };
    let mut system = LiveSystem;
    let path = live.runtime.join(MACHINE_ID_FILE);
    let machine_id = system.generate(&path).unwrap();
    assert_eq!(machine_id, MachineId::Generated(path.clone()));
    let generated = fs::read(&path).unwrap();
    assert_eq!(generated.len(), IMAGE_MACHINE_ID.len());
    assert_ne!(generated, IMAGE_MACHINE_ID);
    let again = system.generate(&path).unwrap_err();
    assert_eq!(again.kind(), io::ErrorKind::AlreadyExists);
    let resolver = live.runtime.join(RESOLVER_FILE);
    system.write_resolver(&resolver, DNS).unwrap();
    let again = system.write_resolver(&resolver, DNS).unwrap_err();
    assert_eq!(again.kind(), io::ErrorKind::AlreadyExists);
    let bound = View {
        resolver: Some(resolver),
        ..view(Some(workload()), machine_id)
    };
    let binds = bound.entry(&live.root).bind_runtime().unwrap();
    let targets = [
        (
            live.root.join("etc/machine-id"),
            generated,
            IMAGE_MACHINE_ID,
        ),
        (
            live.root.join("etc/resolv.conf"),
            b"nameserver 10.0.0.1\n".to_vec(),
            IMAGE_RESOLVER,
        ),
    ];
    for (target, shown, _) in &targets {
        assert_eq!(fs::read(target).unwrap(), *shown, "{}", target.display());
        let writing = fs::OpenOptions::new().write(true).open(target);
        assert_eq!(
            writing.unwrap_err().raw_os_error(),
            Some(Errno::EROFS as i32),
            "{}",
            target.display()
        );
    }

    // The binds keep the root's mount busy, so that the root's teardown, which detaches no busy
    // mount, fails until the binds are removed, which shows the image's own files again.
    let root = sys::path_c_string(&live.root).unwrap();
    assert_eq!(sys::unmount(&root), Err(Errno::EBUSY));
    system.remove(binds).unwrap();
    for (target, _, image) in &targets {
        assert_eq!(fs::read(target).unwrap(), *image, "{}", target.display());
    }
    sys::unmount(&root).unwrap();
}

#[test]
fn the_application_refuses_without_privileges_what_it_cannot_assemble() {
    // A template's runtime directory reaches the assembly, which refuses a relative one before it
    // does anything else.
    let relative = RootTemplate {
        runtime: PathBuf::from("run/nvx"),
        ..template()
    };
    let Err(ApplyError::Assembly(error)) = apply(&accepted(), &relative, None) else {
        panic!("a relative runtime directory was assembled");
    };
    assert!(
        matches!(error.error(), root::Error::Runtime { path } if path == Path::new("run/nvx")),
        "{error}"
    );

    // A GPT disk's partition table is reread before that refusal: on a host without the
    // template's distro device, the reread fails at its one poll, with nothing mounted.
    let disk = clone_record(configuration(
        gpt(None),
        activation::Upper::Tmpfs,
        activation::OverlayMode::TrustedLowers,
        Some(WORKLOAD),
    ));
    let at_once = RootTemplate {
        timeout: Duration::ZERO,
        ..relative
    };
    let Err(ApplyError::Gpt(error)) = apply(&disk, &at_once, None) else {
        panic!("a GPT disk was assembled before its partition table was reread");
    };
    // A host that hides its MMIO windows fails the lookup instead.
    assert!(
        matches!(
            error,
            root::Error::Missing {
                role: root::Role::Distro,
                mmio: DISTRO_MMIO_BASE,
            } | root::Error::Lookup {
                role: root::Role::Distro,
                mmio: DISTRO_MMIO_BASE,
                ..
            }
        ),
        "{error}"
    );
}

#[test]
fn application_errors_name_their_cause() {
    let missing = root::Error::Missing {
        role: root::Role::Distro,
        mmio: DISTRO_MMIO_BASE,
    };
    let refusal = missing.to_string();
    let error = ApplyError::Gpt(missing);
    assert_eq!(
        error.to_string(),
        format!("the reread of the GPT disk's partition table failed: {refusal}")
    );
    assert_eq!(error.source().unwrap().to_string(), refusal);

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

    let exists = io::Error::from(io::ErrorKind::AlreadyExists);
    let error = ApplyError::MachineId {
        error: io::ErrorKind::AlreadyExists.into(),
        teardown: None,
    };
    assert_eq!(
        error.to_string(),
        format!("the workload's machine ID cannot be generated: {exists}")
    );
    assert_eq!(error.source().unwrap().to_string(), exists.to_string());

    let error = ApplyError::View {
        error: entry::Error::Hostname,
        teardown: None,
    };
    assert_eq!(
        error.to_string(),
        "the workload's hostname or machine ID cannot be set: the hostname is invalid"
    );
    assert_eq!(
        error.source().unwrap().to_string(),
        "the hostname is invalid"
    );

    let unmounted = root::Error::Mount {
        mount: root::Mount::Export {
            tag: TAG.to_owned(),
        },
        error: Errno::ENOENT.into(),
    };
    let refusal = unmounted.to_string();
    let error = ApplyError::Export {
        error: unmounted,
        teardown: None,
    };
    assert_eq!(
        error.to_string(),
        format!("the host export cannot be mounted: {refusal}")
    );
    assert_eq!(error.source().unwrap().to_string(), refusal);

    let error = ApplyError::Network {
        error: NetworkError::Write(io::ErrorKind::AlreadyExists.into()),
        teardown: None,
    };
    assert_eq!(
        error.to_string(),
        format!(
            "the network settings cannot be applied: the resolver file cannot be written: {exists}"
        )
    );
    let source = error.source().unwrap();
    assert_eq!(
        source.to_string(),
        format!("the resolver file cannot be written: {exists}")
    );
    assert_eq!(source.source().unwrap().to_string(), exists.to_string());
    let unbound = entry::Error::Resolver(MappingError::TargetKind);
    let refusal = unbound.to_string();
    let error = ApplyError::Network {
        error: NetworkError::Bind(unbound),
        teardown: None,
    };
    assert!(
        refusal.starts_with("cannot bind the resolver file: "),
        "{refusal}"
    );
    assert_eq!(
        error.to_string(),
        format!("the network settings cannot be applied: {refusal}")
    );
    let source = error.source().unwrap();
    assert_eq!(source.to_string(), refusal);
    assert_eq!(
        source.source().unwrap().to_string(),
        MappingError::TargetKind.to_string()
    );
    let error = ApplyError::Network {
        error: NetworkError::Gateway,
        teardown: None,
    };
    assert_eq!(
        error.to_string(),
        "the network settings cannot be applied: the network publishes the workload's listeners, \
         but the template names no IPv4 gateway to announce them to"
    );
    assert!(error.source().unwrap().source().is_none());
    let denied = io::Error::from(Errno::EACCES);
    let error = ApplyError::Network {
        error: NetworkError::Announcer(StartError::Bind(Errno::EACCES.into())),
        teardown: None,
    };
    assert_eq!(
        error.to_string(),
        format!(
            "the network settings cannot be applied: the announcer's socket cannot be bound to a \
             privileged source port: {denied}"
        )
    );
    let source = error.source().unwrap();
    assert_eq!(source.source().unwrap().to_string(), denied.to_string());

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
