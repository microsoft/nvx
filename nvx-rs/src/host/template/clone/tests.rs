//! Tests of the plans of clones: the checks of their images, uppers, exports, and processor and
//! memory targets against the generation that they lease, which read no content, the claims of
//! their scratch images, and the uppers in RAM, whose scratch devices their restores declare
//! absent; in [`launch`], of their launches; and in [`restore`], of their restore sequences.

mod launch;
mod restore;

use std::collections::HashSet;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::*;
use crate::ErrorCode;
use crate::activation::{
    Configuration, ExpectedResources, Export, ExportKind, Identity, Image, MachineIdMode, Network,
    Publication, Upper, WorkloadIdentity,
};
use crate::host::filesystem;
use crate::host::network::{
    Access as NetworkAccess, EgressRule, ForwardProtocol, HostLoopback, LoopbackForward,
    NetworkPolicy, NetworkProxy, Protocol, ProxyScheme,
};
use crate::host::paths;
use crate::host::sandbox::distro::{DISTRO_POLICY_REVISION, PolicyVerdict};
use crate::host::sandbox::scratch::CLAIM_RELEASE_WAIT;
use crate::host::sandbox::{SandboxDistro, SandboxNetwork, SandboxSettings, SandboxUpper};
use crate::host::seal::{FileSeal, open_metadata_only};
use crate::host::store::{
    CLONE_ACTIVATION_NAME, CLONE_BINDING_NAME, CLONE_SCRATCH_NAME, CallerFiles, LOG_NAME,
    SandboxStore,
};
use crate::host::template::store::{CAPTURES_NAME, SNAPSHOT_NAME, TRANSACTION_NAME};
use crate::host::template::test_support::{MIB, ManifestSpec, class, network_class};
use crate::host::template::{CaptureClass, HostTime, KEY_NAME};
#[cfg(windows)]
use crate::host::test_support::junction;
use crate::host::test_support::released;
use crate::protocol::root::RootImage;
use crate::uuid::Uuid;

/// The UUID of the staged images.
const UUID: &str = "11111111-1111-1111-1111-111111111111";
/// The configuration record of these tests' sandboxes.
const CONFIG: &str = "config.json";
/// The length of the template's placeholder, and of the staged images: whole huge pages of 2 MiB,
/// as the length of a file on hugetlbfs is.
const IMAGE_LEN: u64 = 4 * MIB;
/// The length of the template's scratch, and of the staged scratch images: a huge page of 2 MiB.
const SCRATCH_LEN: u64 = 2 * MIB;
/// The template's processors, all online at boot, as [`ManifestSpec::template`] records them.
const PROCESSORS: u32 = 2;
/// The template's RAM, in bytes, as [`ManifestSpec::template`] records it.
const MEMORY: u64 = 512 * MIB;
/// The template's RAM capacity, in bytes, as [`ManifestSpec::template`] records it.
const MEMORY_CAPACITY: u64 = 2048 * MIB;
/// The bound of the first partition of the tests' GPT disks, which the record of a clone of an
/// EROFS layer leaves out.
const FIRST_PARTITION_BOUND: u64 = 64 * 1024;
/// Nanoseconds in a second.
const SECOND: u64 = 1_000_000_000;

/// The access of a host that lets the plan open the clone's files and read their metadata, but
/// fails every read and mapping of their content through what the checks of their bindings
/// open: the image opened for its metadata alone, and the scratch image, on Linux, for writing
/// alone, and on Windows through its guard and the claim's handle, which never ask to read.
const NO_CONTENT: Access<'static> = Access {
    distro: distro::Access {
        open: &open_metadata_only,
        seal: &FileSeal::of,
    },
    scratch: WRITE_ONLY,
};

/// Opens a scratch image for writing alone, as [`NO_CONTENT`] does.
#[cfg(target_os = "linux")]
const WRITE_ONLY: scratch::Access<'static> = scratch::Access {
    open: &open_write_only,
};
/// Opens a scratch image for writing alone, as [`NO_CONTENT`] does.
#[cfg(windows)]
const WRITE_ONLY: scratch::Access<'static> = scratch::Access::WRITABLE;

/// Opens `path` for writing alone, without waiting for a reader of a FIFO.
#[cfg(target_os = "linux")]
fn open_write_only(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    fs::OpenOptions::new()
        .write(true)
        .custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits())
        .open(path)
}

/// Creates the file `path` of `length` bytes, or truncates it to that length, which no one else
/// may change, whatever the umask, as staging callers keep their images.
fn stage(path: &Path, length: u64) {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path).unwrap().set_len(length).unwrap();
}

/// Returns the physical block size that OpenVMM presents for the files of the directory `dir`:
/// the I/O block size of its filesystem on Linux, and 4096 bytes on Windows.
fn physical_block_size(dir: &Path) -> u32 {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;

        let probe = dir.join("probe");
        fs::write(&probe, b"").unwrap();
        let size = fs::metadata(&probe).unwrap().blksize();
        fs::remove_file(&probe).unwrap();
        u32::try_from(size).unwrap()
    }
    #[cfg(windows)]
    {
        let _ = dir;
        4096
    }
}

/// Returns a physical block size other than `physical` that a template may have recorded.
fn other_physical(physical: u32) -> u32 {
    if physical > 512 {
        physical / 2
    } else {
        physical * 2
    }
}

/// Returns the geometry of a block device of `length` bytes, with logical blocks of 512 bytes and
/// physical blocks of `physical` bytes.
fn geometry(length: u64, physical: u32) -> BlockGeometry {
    BlockGeometry {
        length,
        logical_block_size: 512,
        physical_block_size: physical,
    }
}

/// Gives the template of `class`, whose capture `spec` describes, the geometries `distro` and
/// `scratch`.
fn set_geometries(
    class: &mut CaptureClass,
    spec: &mut ManifestSpec,
    distro: BlockGeometry,
    scratch: BlockGeometry,
) {
    class.distro = distro;
    class.scratch = scratch;
    for (block, geometry) in spec.contract().blocks.iter_mut().zip([distro, scratch]) {
        block.length = geometry.length;
        block.logical_block_size = geometry.logical_block_size;
        block.physical_block_size = geometry.physical_block_size;
    }
}

/// Returns the files that these tests' callers keep in their sandboxes.
fn files() -> CallerFiles {
    CallerFiles::new(CONFIG).unwrap()
}

/// Gives the template of `class`, whose capture `spec` describes, a capacity of `processors`
/// processors, of which `boot` are online at boot, as a capture records them.
fn set_processors(class: &mut CaptureClass, spec: &mut ManifestSpec, processors: u32, boot: u32) {
    class.processors = processors;
    class.boot_processors = boot;
    spec.vp_count = processors;
    let contract = spec.contract();
    contract.topology = (1, 1, processors, 1, (0..processors).collect());
    contract.boot_online_vp_count = boot;
}

/// Returns `path` as messages show it.
fn shown(path: &Path) -> String {
    paths::normalized(path).display().to_string()
}

/// Returns the distro layer at `path`.
fn layer(path: &Path) -> SandboxLayer {
    SandboxLayer::new(LayerRole::Distro, path.to_str().unwrap(), UUID)
}

/// Returns the distro GPT disk at `path`, whose disk GUID is [`UUID`].
fn gpt_layer(path: &Path) -> SandboxLayer {
    SandboxLayer::gpt(LayerRole::Distro, path.to_str().unwrap(), UUID)
}

/// Returns the request of a clone of the image at `image`, which `binding` binds, over an upper in
/// RAM, with the template's processors and RAM as its targets, and the settings of [`workload`],
/// for untrusted lowers, and with a journal policy, [`Journal::Forbidden`], that its record leaves
/// out, and for a GPT disk, the bound [`FIRST_PARTITION_BOUND`].
fn tmpfs_request(image: &Path, binding: &DistroBinding) -> CloneRequest {
    CloneRequest {
        adapter: SandboxPlan::adapter(
            SandboxDistro::new(image.to_str().unwrap(), bound_image(binding).unwrap()),
            SandboxUpper::RAM,
        ),
        binding: binding.clone(),
        policy_revision: DISTRO_POLICY_REVISION,
        scratch: None,
        export: None,
        processors: PROCESSORS,
        memory: MEMORY,
        workload: workload(),
        overlay: OverlayMode::UntrustedLowers,
        journal: Journal::Forbidden,
        first_partition_bound: FIRST_PARTITION_BOUND,
    }
}

/// Returns the view that these tests' clones give their workloads of their VMs.
fn workload() -> WorkloadView {
    WorkloadView {
        hostname: b"clone".to_vec(),
        machine_id: MachineIdMode::Generated,
        identity: Some(WorkloadIdentity {
            uid: 1000,
            gid: 1000,
        }),
    }
}

/// Returns the request of a clone of the image at `image`, which `binding` binds, over an upper on
/// the scratch image at `scratch`, which `scratch_binding` binds, with the template's RAM as its
/// memory target.
fn scratch_request(
    image: &Path,
    binding: &DistroBinding,
    scratch: &Path,
    scratch_binding: &ScratchBinding,
) -> CloneRequest {
    let request = tmpfs_request(image, binding);
    CloneRequest {
        adapter: SandboxPlan::adapter(
            request.adapter.distro().clone(),
            SandboxUpper::scratch(scratch.to_str().unwrap()),
        ),
        scratch: Some(scratch_binding.clone()),
        ..request
    }
}

/// Returns the request `request` with the network `network`.
fn with_network(request: CloneRequest, network: Option<SandboxNetwork>) -> CloneRequest {
    CloneRequest {
        adapter: request.adapter.clone().with_network(network),
        ..request
    }
}

/// Returns the network of a guest at `10.0.0.2/24` under a policy whose default is `access`.
fn network(access: NetworkAccess) -> SandboxNetwork {
    SandboxNetwork::new("10.0.0.2/24", NetworkPolicy::new(access)).unwrap()
}

/// Returns the fixture of a template with a NIC, at `10.0.0.2/24`, whose class's network allows
/// egress.
fn network_fixture() -> Fixture {
    Fixture::with(|class, spec, physical| {
        *class = network_class();
        *spec = ManifestSpec::with_network();
        let (distro, scratch) = (
            geometry(IMAGE_LEN, physical),
            geometry(SCRATCH_LEN, physical),
        );
        set_geometries(class, spec, distro, scratch);
    })
}

/// Returns the policies of networks that attach a NIC, a template's of each: egress allowed;
/// egress denied but to some destinations; a proxy alone; ingress allowed; and host-loopback
/// access both ways, with a forward.
fn attaching() -> Vec<NetworkPolicy> {
    let mut ingress = NetworkPolicy::new(NetworkAccess::Deny);
    ingress.ingress = NetworkAccess::Allow;
    let mut forwarded = NetworkPolicy::new(NetworkAccess::Allow);
    forwarded.host_loopback = HostLoopback::Allow;
    forwarded.host_loopback_forwards.push(LoopbackForward {
        protocol: ForwardProtocol::Tcp,
        host_port: 18080,
        guest_port: 8080,
    });
    vec![
        NetworkPolicy::new(NetworkAccess::Allow),
        NetworkPolicy::new(NetworkAccess::Deny)
            .with_allow(EgressRule::to("192.0.2.0/24").on_port(Protocol::Tcp, 443)),
        NetworkPolicy::proxy_only(NetworkProxy::new(3128).with_scheme(ProxyScheme::Http)),
        ingress,
        forwarded,
    ]
}

/// Returns the request `request` with the export `export`.
fn with_export(request: CloneRequest, export: &ExportPlan) -> CloneRequest {
    CloneRequest {
        export: Some(export.clone()),
        ..request
    }
}

/// Plans the export of `mappings`, host paths each at its guest target, read-only or not, for the
/// container root, as an adapter's sandbox plans it, with the paths `denied` hidden, and the paths
/// `allowed` inside them shown again.
fn export_plan(
    mappings: &[(&Path, &str, bool)],
    denied: &[PathBuf],
    allowed: &[PathBuf],
) -> ExportPlan {
    let mappings: Vec<filesystem::Mapping<'_>> = mappings
        .iter()
        .map(|&(path, target, read_only)| filesystem::Mapping {
            name: "mappings",
            path,
            target: filesystem::Target::Guest(target),
            read_only,
        })
        .collect();
    let request = filesystem::ExportRequest {
        denied: filesystem::PathList {
            name: "denied",
            paths: denied,
        },
        allowed: filesystem::PathList {
            name: "allowed",
            paths: allowed,
        },
        ..filesystem::ExportRequest::new(&mappings, filesystem::ReservedTargets::CONTAINER_ROOT)
    };
    filesystem::plan(&request).unwrap().unwrap()
}

/// Returns another spelling of the existing directory `directory`, which names it, but which
/// differs from the path that resolves it: through a directory in it and its `..` on Unix, where
/// an absolute path keeps its `..` components, and with the verbatim prefix on Windows.
fn alias(directory: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let resolved = filesystem::canonicalize(directory).unwrap();
        PathBuf::from(format!(r"\\?\{}", resolved.display()))
    }
    #[cfg(not(windows))]
    {
        let inner = directory.join("alias");
        fs::create_dir(&inner).unwrap();
        inner.join("..")
    }
}

/// Returns the mapping table of these tests' exports, [`Fixture::data_export`], as a cold launch's
/// guest receives it: the exported directory, child 0, and the directory `work` in it, each by its
/// path in the export, which names the child's index.
fn data_table() -> [HostMap; 2] {
    [
        HostMap {
            source: "0".to_owned(),
            target: "/data".to_owned(),
            read_only: true,
        },
        HostMap {
            source: "0/work".to_owned(),
            target: "/work".to_owned(),
            read_only: false,
        },
    ]
}

/// Returns the kind and the message of the error of `result`, a clone's plan that failed with
/// `evidence`, as [`refused_at`] checks it.
fn refused<T: fmt::Debug>(
    result: std::result::Result<T, CloneFailure>,
    evidence: Evidence,
) -> (ErrorKind, String) {
    refused_at(result, Stage::Plan, evidence)
}

/// Returns the kind and the message of the error of `result`, which must have failed at `stage`,
/// the plan or the launch, with `evidence`, before anything of the launch existed: with the
/// transaction of the clone's restore once the plan drew it, and no VM to tear down.
fn refused_at<T: fmt::Debug>(
    result: std::result::Result<T, CloneFailure>,
    stage: Stage,
    evidence: Evidence,
) -> (ErrorKind, String) {
    let failure = result.unwrap_err();
    let record = failure.record();
    assert_eq!(
        (record.stage, record.evidence, failure.outcome()),
        (stage, evidence, evidence.outcome()),
        "{failure}"
    );
    assert_eq!(
        (record.progress, failure.teardown(), record.status),
        (Progress::Planned, &Teardown::NoVm, None),
        "{failure}"
    );
    assert_eq!(
        record.transaction.is_some(),
        stage != Stage::Plan,
        "{failure}"
    );
    assert_eq!(record.code, failure.code());
    let error = failure.into_error();
    (error.kind(), error.to_string())
}

/// Returns the time of the host that captured `generation`, during the boot of the capture,
/// `seconds` after it.
fn after(generation: &Generation, seconds: u64) -> HostTime {
    let time = generation.time();
    HostTime {
        host_id: time.host_id,
        boot_id: time.boot_id,
        clock: time.host_clock,
        monotonic_ns: time.capture_monotonic_ns + seconds * SECOND,
        utc_ns: time.capture_utc_ns + seconds * SECOND,
    }
}

/// A template store in a temporary directory, with a published generation whose `distro` and
/// scratch have the geometries of the files of the directory, and the directories of the staged
/// images, scratch images, and sandboxes.
struct Fixture {
    /// The temporary directory.
    directory: tempfile::TempDir,
    /// The store.
    store: Store,
    /// The published generation.
    generation: Generation,
    /// The physical block size of the files of the directory.
    physical: u32,
}

impl Fixture {
    /// Publishes a template whose `distro` is [`IMAGE_LEN`] bytes long and whose scratch is
    /// [`SCRATCH_LEN`] bytes long, both with the physical block size of the files of the
    /// directory.
    fn new() -> Self {
        Self::with(|_, _, _| {})
    }

    /// Publishes a template as [`new`](Self::new) does, of the class and manifest that `change`
    /// changes, given the physical block size of the files of the directory.
    fn with(change: impl FnOnce(&mut CaptureClass, &mut ManifestSpec, u32)) -> Self {
        Self::in_store(|directory| directory.join("templates"), change)
    }

    /// Publishes a template as [`with`](Self::with) does, in a store at the path that `store`
    /// returns, given the temporary directory, in which it creates the store's parent directory.
    fn in_store(
        store: impl FnOnce(&Path) -> PathBuf,
        change: impl FnOnce(&mut CaptureClass, &mut ManifestSpec, u32),
    ) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let physical = physical_block_size(directory.path());
        let store = Store::create(store(directory.path())).unwrap();
        let (mut class, mut spec) = (class(), ManifestSpec::template());
        set_geometries(
            &mut class,
            &mut spec,
            geometry(IMAGE_LEN, physical),
            geometry(SCRATCH_LEN, physical),
        );
        change(&mut class, &mut spec, physical);
        let generation = store.publish_spec_for_tests(&class, &spec);
        // Staging callers keep images in directories that only their own account can write.
        for name in ["images", "scratches", "sandboxes"] {
            let dir = directory.path().join(name);
            assert_eq!(platform::create_owner_only_dir(&dir).unwrap(), None);
        }
        Self {
            directory,
            store,
            generation,
            physical,
        }
    }

    /// Returns the path of the file `name` of the directory `kind`.
    fn path(&self, kind: &str, name: &str) -> PathBuf {
        self.directory.path().join(kind).join(name)
    }

    /// Stages the image `name` of `length` bytes, and returns its path and its binding.
    fn image(&self, name: &str, length: u64) -> (PathBuf, DistroBinding) {
        let path = self.path("images", name);
        stage(&path, length);
        let binding = DistroBinding::import(&layer(&path), PolicyVerdict::ACCEPTED).unwrap();
        (path, binding)
    }

    /// Stages the GPT disk `name` of `length` bytes, whose disk GUID is [`UUID`], as
    /// [`image`](Self::image) stages an EROFS layer, and returns its path and its binding.
    fn gpt_image(&self, name: &str, length: u64) -> (PathBuf, DistroBinding) {
        let path = self.path("images", name);
        stage(&path, length);
        let binding = DistroBinding::import(&gpt_layer(&path), PolicyVerdict::ACCEPTED).unwrap();
        (path, binding)
    }

    /// Stages the scratch image `name` of `length` bytes, and returns its path and its binding.
    fn scratch(&self, name: &str, length: u64) -> (PathBuf, ScratchBinding) {
        let path = self.path("scratches", name);
        stage(&path, length);
        let binding = ScratchBinding::bind(&path).unwrap();
        (path, binding)
    }

    /// Creates the directory `name` among the exported directories, with the directories `inside`
    /// it, and returns its path, with its links resolved, as an export plan names it.
    fn exported(&self, name: &str, inside: &[&str]) -> PathBuf {
        let dir = self.path("exports", name);
        fs::create_dir_all(&dir).unwrap();
        for path in inside {
            fs::create_dir_all(dir.join(path)).unwrap();
        }
        filesystem::canonicalize(&dir).unwrap()
    }

    /// Plans the export of the exported directory `name`, which the guest reads at `/data`, and
    /// in which it writes the directory `work`, at `/work`, as [`data_table`] maps them: an
    /// aggregate of one directory, whose writes the plan narrows to `work`.
    fn data_export(&self, name: &str) -> ExportPlan {
        let data = self.exported(name, &["work"]);
        export_plan(
            &[(&data, "/data", true), (&data.join("work"), "/work", false)],
            &[],
            &[],
        )
    }

    /// Plans the export of an aggregate of the directory `name` among the exported directories, as
    /// a cold launch exports one, and returns it with its directories and files, in the order of
    /// their children: the directory `data`, which the guest reads at `/data`, but for its
    /// directory `work`, which it writes at `/work`, and which hides its directory `secret`, but
    /// for the directory `shown` in it; the file `notes.txt`, which the guest writes at
    /// `/notes.txt`; the file `settings.json`, which it reads at `/settings.json`; and the
    /// directory `tools`, which it reads at `/tools`.
    fn aggregate_export(&self, name: &str) -> (ExportPlan, [PathBuf; 4]) {
        let root = self.exported(name, &["data/work", "data/secret/shown", "tools"]);
        let [data, notes, settings, tools] =
            ["data", "notes.txt", "settings.json", "tools"].map(|entry| root.join(entry));
        for file in [&notes, &settings] {
            fs::write(file, b"file").unwrap();
        }
        let secret = data.join("secret");
        let export = export_plan(
            &[
                (&data, "/data", true),
                (&data.join("work"), "/work", false),
                (&notes, "/notes.txt", false),
                (&settings, "/settings.json", true),
                (&tools, "/tools", true),
            ],
            std::slice::from_ref(&secret),
            &[secret.join("shown")],
        );
        (export, [data, notes, settings, tools])
    }

    /// Provisions the sandbox `name`, and returns its directory, with its links resolved, as a
    /// lease names it.
    fn provision(&self, name: &str) -> PathBuf {
        let dir = self.path("sandboxes", name);
        let locked = SandboxStore::lock_for_provision(&dir, &files(), None).unwrap();
        fs::write(dir.join(CONFIG), b"{}").unwrap();
        drop(locked);
        filesystem::canonicalize(&dir).unwrap()
    }

    /// Provisions the sandbox `name`, and leases the generation to it, a second after its
    /// capture.
    fn acquire(&self, name: &str) -> Acquired {
        let sandbox = self.provision(name);
        self.store
            .acquire(
                self.generation.generation(),
                sandbox,
                &files(),
                &after(&self.generation, 1),
                None,
            )
            .unwrap()
    }

    /// Plans the clone of `acquired` as `request` asks, through `access`, waiting
    /// [`CLAIM_RELEASE_WAIT`] at most for the claim of its scratch image.
    fn plan(
        &self,
        acquired: &mut Acquired,
        request: &CloneRequest,
        access: Access<'_>,
    ) -> Result<ClonePlan, CloneFailure> {
        ClonePlan::new_with(
            &self.store,
            acquired,
            request,
            Instant::now() + CLAIM_RELEASE_WAIT,
            access,
        )
    }
}

/// Returns the names of the entries of the directory `dir`, in order.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// Asserts that the sandbox of `acquired` holds no record of a clone's progress or failure, and
/// that its lease still reserves its launch, as after a plan that failed.
fn assert_untouched(acquired: &Acquired) {
    let locked = &acquired.locked;
    for path in [locked.clone_progress_path(), locked.clone_failure_path()] {
        assert!(!path.exists(), "{}", path.display());
    }
    assert_eq!(locked.reserved(), Some(acquired.lease.launch()));
}

/// Returns a file of `length` bytes on hugetlbfs, whose I/O block size is a huge page of 2 MiB,
/// or `None`, saying why, if the host offers none.
#[cfg(target_os = "linux")]
fn huge_file(length: u64) -> Option<File> {
    use nix::sys::memfd::{MFdFlags, memfd_create};

    let flags = MFdFlags::MFD_CLOEXEC | MFdFlags::MFD_HUGETLB | MFdFlags::MFD_HUGE_2MB;
    let file = match memfd_create("nvx-clone-test", flags) {
        Ok(descriptor) => File::from(descriptor),
        Err(error) => {
            eprintln!("this host offers no file on hugetlbfs: {error}");
            return None;
        }
    };
    if let Err(error) = file.set_len(length) {
        eprintln!("this host cannot size a file on hugetlbfs: {error}");
        return None;
    }
    Some(file)
}

/// Links `link` to the file `target` with a symbolic link, and returns whether it could: Windows
/// lets only Developer Mode or a privilege create one.
fn symlink_file(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_file(target, link);
    created_link(linked)
}

/// Links `link` to the directory `target` with a symbolic link, and returns whether it could, as
/// [`symlink_file`] does.
fn symlink_dir(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_dir(target, link);
    created_link(linked)
}

/// Returns whether a symbolic link was created, as `linked` reports, and fails unless Windows
/// refused it.
fn created_link(linked: io::Result<()>) -> bool {
    match linked {
        Ok(()) => true,
        Err(error) if cfg!(windows) => {
            eprintln!("cannot create a symbolic link: {error}");
            false
        }
        Err(error) => panic!("cannot create a symbolic link: {error}"),
    }
}

/// Links `link` to the directory `target`: a symbolic link on Unix, and a junction on Windows,
/// which any user may create.
fn link_dir(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    junction(link, target);
}

#[test]
fn a_clone_in_ram_restores_its_lease_with_its_image_and_no_scratch_file() {
    let fixture = Fixture::new();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let sandbox = acquired.locked.dir().to_owned();
    let before = entries(&sandbox);
    let request = tmpfs_request(&image, &binding);
    let plan = fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap();
    let store = &fixture.store;
    assert_eq!(plan.lease(), &acquired.lease);
    assert_eq!(plan.generation(), &fixture.generation);
    assert_eq!(plan.key(), &store.read_key(&fixture.generation).unwrap());
    assert_eq!(
        plan.snapshot(),
        store.snapshot_dir(&fixture.generation).unwrap().as_path()
    );
    assert_eq!(plan.memory(), MEMORY);
    // The image is the file that its binding's check opened, which reads nothing.
    assert_eq!(plan.image().path().to_str().unwrap(), shown(&image));
    assert_eq!(
        plan.image().geometry().unwrap(),
        geometry(IMAGE_LEN, fixture.physical)
    );
    let mut byte = [0];
    assert!(plan.image().file().read(&mut byte).is_err());
    // The restore declares the `scratch` block device of an upper in RAM absent, so the plan has
    // no file for it, and creates nothing in the sandbox's directory.
    assert!(matches!(plan.upper(), CloneUpper::Tmpfs));
    assert_eq!(plan.scratch_path(), None);
    assert_eq!(entries(&sandbox), before);
    // The capability is fresh, and never shown.
    let capability = *plan.capability();
    assert_ne!(capability, [0; CAPABILITY_LEN]);
    let shown_plan = format!("{plan:?}");
    assert!(shown_plan.starts_with("ClonePlan {"), "{shown_plan}");
    assert!(
        !shown_plan.contains(&format!("{capability:?}")),
        "{shown_plan}"
    );
    assert!(!shown_plan.contains("capability"), "{shown_plan}");
    // The plan borrows nothing of the acquisition, whose lease still reserves its launch.
    assert_eq!(acquired.locked.reserved(), Some(acquired.lease.launch()));
    drop(plan);
    // The access of launches plans alike, with another capability.
    let again = ClonePlan::new(store, &mut acquired, &request, Instant::now()).unwrap();
    assert_ne!(*again.capability(), capability);
    assert_eq!(again.scratch_path(), None);
    assert_eq!(entries(&sandbox), before);
}

#[test]
fn files_of_the_sandboxs_store_are_refused_as_images() {
    let fixture = Fixture::new();
    let mut acquired = fixture.acquire("sandbox");
    let record = acquired.locked.clone_activation_path();
    // The image or the scratch image of a request is a file of the sandbox's store: the request's
    // settings are invalid.
    let store_file = |result: std::result::Result<ClonePlan, CloneFailure>| {
        refused(result, Evidence::InvalidSettings)
    };
    let refusal = |what: &str, path: &Path, owned: &Path| {
        (
            ErrorKind::Unsupported,
            format!(
                "the {what} {} is {}, a file of the sandbox's store, which its transitions \
                 replace or remove",
                shown(path),
                shown(owned)
            ),
        )
    };
    let seal_of = |path: &Path| FileSeal::of(&File::open(path).unwrap()).unwrap();
    // An image that is the clone's activation record, which a launch would replace, is refused
    // before anything replaces it...
    stage(&record, IMAGE_LEN);
    let binding = DistroBinding::import(&layer(&record), PolicyVerdict::ACCEPTED).unwrap();
    let staged = seal_of(&record);
    assert_eq!(
        store_file(fixture.plan(&mut acquired, &tmpfs_request(&record, &binding), NO_CONTENT)),
        refusal("image", &record, &record)
    );
    assert_eq!(seal_of(&record), staged);
    // So is the scratch device's file that an earlier version left for a clone in RAM, which a
    // release removes.
    let left = acquired.locked.dir().join(CLONE_SCRATCH_NAME);
    stage(&left, IMAGE_LEN);
    let left_binding = DistroBinding::import(&layer(&left), PolicyVerdict::ACCEPTED).unwrap();
    assert_eq!(
        store_file(fixture.plan(
            &mut acquired,
            &tmpfs_request(&left, &left_binding),
            NO_CONTENT
        )),
        refusal("image", &left, &left)
    );
    fs::remove_file(&left).unwrap();
    // ...by whatever path the request names it: through a hard link...
    let hard = fixture.path("images", "hard.erofs");
    fs::hard_link(&record, &hard).unwrap();
    let hard_binding = DistroBinding::import(&layer(&hard), PolicyVerdict::ACCEPTED).unwrap();
    assert_eq!(
        store_file(fixture.plan(
            &mut acquired,
            &tmpfs_request(&hard, &hard_binding),
            NO_CONTENT
        )),
        refusal("image", &hard, &record)
    );
    // ...or through a link among its directories.
    let alias = fixture.path("images", "alias");
    if symlink_dir(acquired.locked.dir(), &alias) {
        let aliased = alias.join(CLONE_ACTIVATION_NAME);
        let aliased_binding =
            DistroBinding::import(&layer(&aliased), PolicyVerdict::ACCEPTED).unwrap();
        assert_eq!(
            store_file(fixture.plan(
                &mut acquired,
                &tmpfs_request(&aliased, &aliased_binding),
                NO_CONTENT
            )),
            refusal("image", &aliased, &record)
        );
    }
    // A release of the lease removes that file, so a clone on a scratch image takes it neither
    // as its image, which is bound again, since the hard link changed its change time on
    // Linux...
    let binding = DistroBinding::import(&layer(&record), PolicyVerdict::ACCEPTED).unwrap();
    let staged = seal_of(&record);
    let (image, image_binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let (scratch, scratch_binding) = fixture.scratch("scratch.ext4", SCRATCH_LEN);
    let request = scratch_request(&record, &binding, &scratch, &scratch_binding);
    assert_eq!(
        store_file(fixture.plan(&mut acquired, &request, NO_CONTENT)),
        refusal("image", &record, &record)
    );
    assert_eq!(seal_of(&record), staged);
    // ...nor as its scratch image, whose claim the refused plan releases.
    fs::remove_file(&record).unwrap();
    stage(&record, SCRATCH_LEN);
    let record_binding = ScratchBinding::bind(&record).unwrap();
    let request = scratch_request(&image, &image_binding, &record, &record_binding);
    assert_eq!(
        store_file(fixture.plan(&mut acquired, &request, NO_CONTENT)),
        refusal("scratch image", &record, &record)
    );
    assert_eq!(fs::metadata(&record).unwrap().len(), SCRATCH_LEN);
    assert!(released(|| record_binding
        .check(&record, Instant::now())
        .is_ok()));
    fs::remove_file(&record).unwrap();
    // The store's other files are refused alike, such as its log, which a launch replaces, and
    // the temporary files of its records.
    for name in [LOG_NAME, ".runtime.json.1.tmp"] {
        let owned = acquired.locked.dir().join(name);
        stage(&owned, IMAGE_LEN);
        let binding = DistroBinding::import(&layer(&owned), PolicyVerdict::ACCEPTED).unwrap();
        assert_eq!(
            store_file(fixture.plan(&mut acquired, &tmpfs_request(&owned, &binding), NO_CONTENT)),
            refusal("image", &owned, &owned)
        );
        assert_eq!(fs::metadata(&owned).unwrap().len(), IMAGE_LEN);
        fs::remove_file(&owned).unwrap();
    }
    assert_untouched(&acquired);
}

#[test]
fn links_and_directories_under_the_names_of_the_sandboxs_store_are_refused() {
    let fixture = Fixture::new();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let record = acquired.locked.clone_activation_path();
    // The sandbox's store holds an entry that it never creates: the host's state is not as the
    // store keeps it.
    let store_entry = |result: std::result::Result<ClonePlan, CloneFailure>| {
        refused(result, Evidence::HostFailure)
    };
    let refusal = |owned: &Path| {
        (
            ErrorKind::Unavailable,
            format!(
                "{} is a link or a directory, which the sandbox's store never creates there",
                shown(owned)
            ),
        )
    };
    let request = tmpfs_request(&image, &binding);
    // A directory, which a launch could not replace, nor a release remove.
    for name in [CLONE_ACTIVATION_NAME, LOG_NAME] {
        let owned = acquired.locked.dir().join(name);
        fs::create_dir(&owned).unwrap();
        assert_eq!(
            store_entry(fixture.plan(&mut acquired, &request, NO_CONTENT)),
            refusal(&owned)
        );
        assert!(owned.is_dir());
        fs::remove_dir(&owned).unwrap();
    }
    // A link to the directory of the images, through which the path of an image can pass, and
    // which a launch would replace, leaving that path naming nothing.
    if symlink_dir(image.parent().unwrap(), &record) {
        let through = record.join("distro.erofs");
        let through_binding =
            DistroBinding::import(&layer(&through), PolicyVerdict::ACCEPTED).unwrap();
        for request in [tmpfs_request(&through, &through_binding), request.clone()] {
            assert_eq!(
                store_entry(fixture.plan(&mut acquired, &request, NO_CONTENT)),
                refusal(&record)
            );
        }
        assert!(
            fs::symlink_metadata(&record)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(fs::metadata(&through).unwrap().is_file());
        #[cfg(unix)]
        fs::remove_file(&record).unwrap();
        #[cfg(windows)]
        fs::remove_dir(&record).unwrap();
    }
    assert_untouched(&acquired);
    // A control socket that a launch left is no image, and the launch removes it.
    #[cfg(unix)]
    let _socket = std::os::unix::net::UnixListener::bind(
        acquired.locked.dir().join(crate::host::store::SOCKET_NAME),
    )
    .unwrap();
    // Once nothing else is there, the clone is planned.
    let plan = fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap();
    assert_eq!(plan.scratch_path(), None);
}

#[test]
fn a_scratch_image_that_is_a_file_of_a_generation_s_snapshot_is_refused() {
    let fixture = Fixture::new();
    let (image, image_binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    // The RAM of the leased generation's snapshot, and that of another generation's, which the
    // store's index need not list, of the template's scratch length: the clone's guest would write
    // a snapshot that every clone of its generation restores in place.
    let snapshot = fixture.store.snapshot_dir(&fixture.generation).unwrap();
    let leased = snapshot.join("memory.bin");
    stage(&leased, SCRATCH_LEN);
    let next = fixture.generation.generation().get().get() + 1;
    let other = crate::activation::StoreGeneration::new(std::num::NonZeroU64::new(next).unwrap());
    let other = fixture
        .store
        .payload_dir(other)
        .join(snapshot.file_name().unwrap());
    fs::create_dir_all(&other).unwrap();
    let other = other.join("memory.bin");
    stage(&other, SCRATCH_LEN);
    // Through a hard link among the scratch images, another name of the file, and by its own
    // path, in the store's directory: the request's settings are invalid.
    let cases = [
        (&leased, fixture.path("scratches", "leased.raw")),
        (&other, fixture.path("scratches", "other.raw")),
        (&other, other.clone()),
    ];
    for (memory, scratch) in cases {
        let linked = scratch != *memory;
        if linked {
            fs::hard_link(memory, &scratch).unwrap();
        }
        let binding = ScratchBinding::bind(&scratch).unwrap();
        let request = scratch_request(&image, &image_binding, &scratch, &binding);
        let expected = if linked {
            format!(
                "the scratch image {} has 2 names, but a clone's scratch image must have a single \
                 name: another, a hard link, could make it a file of a snapshot, which every \
                 clone of its generation restores in place",
                scratch.display()
            )
        } else {
            format!(
                "the scratch image {} is {}, in the directory of the template store {}, where its \
                 captures write the snapshots that every clone of a generation restores in place: \
                 a clone's scratch image must be outside it",
                scratch.display(),
                location_of(&File::open(&scratch).unwrap())
                    .unwrap()
                    .display(),
                fixture.store.dir().display()
            )
        };
        assert_eq!(
            refused(
                fixture.plan(&mut acquired, &request, NO_CONTENT),
                Evidence::InvalidSettings
            ),
            (ErrorKind::Unsupported, expected)
        );
        assert_untouched(&acquired);
    }
}

#[test]
fn a_scratch_image_in_the_template_store_is_refused() {
    let fixture = Fixture::new();
    let (image, image_binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    // A file in the store's directory, and one of a snapshot that a capture of the store writes,
    // which its publication makes a generation's by renaming the capture's transaction, of the
    // template's scratch length: by their own paths, and through links on the way to them, which
    // the image's location resolves.
    let dir = fixture.store.dir().to_owned();
    let loose = dir.join("scratch.raw");
    let transaction = dir.join(CAPTURES_NAME).join("slot").join(TRANSACTION_NAME);
    let memory = transaction.join(SNAPSHOT_NAME).join("memory.bin");
    fs::create_dir_all(memory.parent().unwrap()).unwrap();
    for file in [&loose, &memory] {
        stage(file, SCRATCH_LEN);
    }
    let through = |link: &Path| link.join(SNAPSHOT_NAME).join("memory.bin");
    let mut cases = vec![(&loose, loose.clone()), (&memory, memory.clone())];
    let link = fixture.path("links", "transaction");
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&transaction, &link).unwrap();
    #[cfg(windows)]
    junction(&link, &transaction);
    cases.push((&memory, through(&link)));
    // The transaction held open, as a descriptor keeps a directory whatever renames it.
    #[cfg(target_os = "linux")]
    let held = File::open(&transaction).unwrap();
    #[cfg(target_os = "linux")]
    cases.push((
        &memory,
        through(
            &Path::new("/proc/self/fd").join(std::os::fd::AsRawFd::as_raw_fd(&held).to_string()),
        ),
    ));
    for (file, scratch) in cases {
        let binding = ScratchBinding::bind(&scratch).unwrap();
        let request = scratch_request(&image, &image_binding, &scratch, &binding);
        let location = location_of(&File::open(file).unwrap()).unwrap();
        assert_eq!(
            refused(
                fixture.plan(&mut acquired, &request, NO_CONTENT),
                Evidence::InvalidSettings
            ),
            (
                ErrorKind::Unsupported,
                format!(
                    "the scratch image {} is {}, in the directory of the template store {}, \
                     where its captures write the snapshots that every clone of a generation \
                     restores in place: a clone's scratch image must be outside it",
                    scratch.display(),
                    location.display(),
                    dir.display()
                )
            )
        );
        assert_untouched(&acquired);
    }
}

/// On Windows, a file of a volume without a drive letter or a mount point is located by the
/// volume's GUID, above which the store's directory is found all the same.
#[cfg(windows)]
#[test]
fn locations_that_name_their_volume_by_its_guid_are_compared_with_the_store() {
    let fixture = Fixture::new();
    let dir = fixture.store.dir().to_owned();
    let inside = dir
        .join(CAPTURES_NAME)
        .join("slot")
        .join(TRANSACTION_NAME)
        .join("memory.bin");
    fs::create_dir_all(inside.parent().unwrap()).unwrap();
    stage(&inside, SCRATCH_LEN);
    let (outside, _) = fixture.scratch("scratch.ext4", SCRATCH_LEN);
    for (file, within) in [(&inside, true), (&outside, false)] {
        let location = crate::host::seal::final_volume_path(&File::open(file).unwrap()).unwrap();
        assert!(
            location.to_string_lossy().starts_with(r"\\?\Volume{"),
            "{}",
            location.display()
        );
        assert_eq!(
            is_within(&location, &dir).unwrap(),
            within,
            "{}",
            location.display()
        );
    }
}

#[test]
fn a_scratch_image_with_another_name_is_refused() {
    let fixture = Fixture::new();
    let (image, image_binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    // Another name of the image, outside the template store, as a file of the snapshot of another
    // store would be, or one of a snapshot that a capture of this store writes, which neither the
    // store's generations nor the image's location show.
    let (scratch, _) = fixture.scratch("scratch.ext4", SCRATCH_LEN);
    let other = fixture.path("stores", "memory.bin");
    fs::create_dir_all(other.parent().unwrap()).unwrap();
    fs::hard_link(&scratch, &other).unwrap();
    let binding = ScratchBinding::bind(&scratch).unwrap();
    let request = scratch_request(&image, &image_binding, &scratch, &binding);
    assert_eq!(
        refused(
            fixture.plan(&mut acquired, &request, NO_CONTENT),
            Evidence::InvalidSettings
        ),
        (
            ErrorKind::Unsupported,
            format!(
                "the scratch image {} has 2 names, but a clone's scratch image must have a single \
                 name: another, a hard link, could make it a file of a snapshot, which every clone \
                 of its generation restores in place",
                scratch.display()
            )
        )
    );
    assert_untouched(&acquired);
    // Once its other name went, the image is the clone's alone.
    fs::remove_file(&other).unwrap();
    drop(fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap());
}

#[test]
fn names_that_the_file_system_takes_for_those_of_the_stores_files_are_the_stores() {
    let fixture = Fixture::new();
    let (image, image_binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let dir = acquired.locked.dir().to_owned();
    // The image or the scratch image of a request is a file of the sandbox's store: the request's
    // settings are invalid.
    let store_file = |result: std::result::Result<ClonePlan, CloneFailure>| {
        refused(result, Evidence::InvalidSettings)
    };
    let refusal = |what: &str, path: &Path, owned: &Path| {
        (
            ErrorKind::Unsupported,
            format!(
                "the {what} {} is {}, a file of the sandbox's store, which its transitions \
                 replace or remove",
                shown(path),
                shown(owned)
            ),
        )
    };
    // Whether the file system takes the paths `a` and `b` for one file.
    let same_file = |a: &Path, b: &Path| {
        let identity = |path: &Path| {
            File::open(path)
                .and_then(|file| FileSeal::of(&file))
                .map(|seal| (seal.device(), seal.file_id()))
                .ok()
        };
        identity(a).is_some() && identity(a) == identity(b)
    };
    // An image under the name of one of the store's files in other letter cases is that file
    // where the file system ignores case, as Windows' do, and another file where it does not.
    for name in [CLONE_ACTIVATION_NAME, LOG_NAME] {
        let cased = dir.join(name.to_ascii_uppercase());
        stage(&cased, IMAGE_LEN);
        let binding = DistroBinding::import(&layer(&cased), PolicyVerdict::ACCEPTED).unwrap();
        let owned = dir.join(name);
        let same = same_file(&cased, &owned);
        let result = fixture.plan(&mut acquired, &tmpfs_request(&cased, &binding), NO_CONTENT);
        if same {
            assert_eq!(store_file(result), refusal("image", &cased, &owned));
        } else {
            drop(result.unwrap());
        }
        assert_eq!(fs::metadata(&cased).unwrap().len(), IMAGE_LEN);
        fs::remove_file(&cased).unwrap();
    }
    // So is a scratch image, which a release would remove as the store's file where the file
    // system takes it for that file, and keeps where it does not.
    let record = acquired.locked.clone_activation_path();
    let cased = dir.join(CLONE_ACTIVATION_NAME.to_ascii_uppercase());
    stage(&cased, SCRATCH_LEN);
    let binding = ScratchBinding::bind(&cased).unwrap();
    let request = scratch_request(&image, &image_binding, &cased, &binding);
    let same = same_file(&cased, &record);
    let result = fixture.plan(&mut acquired, &request, NO_CONTENT);
    if same {
        assert_eq!(
            store_file(result),
            refusal("scratch image", &cased, &record)
        );
    } else {
        drop(result.unwrap());
        assert_eq!(
            fixture.store.release(&mut acquired.locked).unwrap(),
            Some(acquired.lease.clone())
        );
    }
    assert_eq!(fs::metadata(&cased).unwrap().len(), SCRATCH_LEN);
}

#[test]
fn templates_that_a_runtime_of_other_contracts_captured_are_refused() {
    // A store that a build of other boot or control contracts kept holds templates whose guests
    // this build cannot drive.
    type ClassChange = fn(&mut CaptureClass);
    let changes: [(&str, ClassChange); 3] = [
        ("boot contract", |class| class.runtime.boot_contract += 1),
        ("control contract", |class| {
            class.runtime.control_contract = "other-revision".to_owned();
        }),
        ("control session", |class| {
            class.runtime.control_session += 1
        }),
    ];
    for (name, change) in changes {
        let fixture = Fixture::with(|class, _, _| change(class));
        let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
        let mut acquired = fixture.acquire("sandbox");
        let plan = fixture.plan(&mut acquired, &tmpfs_request(&image, &binding), NO_CONTENT);
        assert_eq!(
            refused(plan, Evidence::IncompatibleHost),
            (
                ErrorKind::Unsupported,
                format!(
                    "the template's generation {} speaks other contracts than this build, which \
                     restores none of its clones",
                    fixture.generation.generation()
                )
            ),
            "{name}"
        );
        assert_untouched(&acquired);
    }
}

#[test]
fn images_that_their_binding_does_not_authorize_and_malformed_generations_are_refused() {
    let fixture = Fixture::new();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let request = tmpfs_request(&image, &binding);
    // A binding whose verdict rejects the image authorizes no clone of it.
    let verdict = PolicyVerdict {
        result: PolicyResult::Rejected,
        ..PolicyVerdict::ACCEPTED
    };
    let rejected = DistroBinding::import(&layer(&image), verdict).unwrap();
    let plan = fixture.plan(&mut acquired, &tmpfs_request(&image, &rejected), NO_CONTENT);
    refused(plan, Evidence::AuthorizationDenied);
    // A binding of another revision of the distro policy than the request's is stale.
    let stale = CloneRequest {
        policy_revision: DISTRO_POLICY_REVISION + 1,
        ..request.clone()
    };
    refused(
        fixture.plan(&mut acquired, &stale, NO_CONTENT),
        Evidence::StaleBinding,
    );
    // A failure of the request quarantines nothing.
    let failure = fixture.plan(&mut acquired, &stale, NO_CONTENT).unwrap_err();
    assert!(!failure.quarantine(&fixture.store, None).unwrap());
    assert_untouched(&acquired);
    // A generation whose key is gone is malformed: a failure of the template, whose generation it
    // quarantines through its tombstone, once the sandbox's lock is released, as the lock order
    // asks.
    let generation = fixture.generation.generation();
    let key = fixture
        .store
        .payload_dir(generation)
        .join(crate::host::template::KEY_NAME);
    fs::remove_file(key).unwrap();
    let failure = fixture
        .plan(&mut acquired, &request, NO_CONTENT)
        .unwrap_err();
    assert_eq!(
        (failure.record().evidence, failure.outcome()),
        (Evidence::MalformedGeneration, Outcome::Template),
        "{failure}"
    );
    assert_untouched(&acquired);
    drop(acquired);
    let other = Fixture::new();
    let error = failure.quarantine(&other.store, None).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Other, "{error}");
    assert!(other.store.tombstones().unwrap().is_empty());
    assert!(failure.quarantine(&fixture.store, None).unwrap());
    assert_eq!(fixture.store.tombstones().unwrap(), [generation]);
    // The generation takes no new lease.
    let error = fixture
        .store
        .acquire(
            generation,
            fixture.provision("next"),
            &files(),
            &after(&fixture.generation, 1),
            None,
        )
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::TemplateTombstoned, "{error}");
}

#[test]
fn keys_that_their_read_refuses_for_what_they_are_are_failures_of_the_template() {
    let fixture = Fixture::new();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let request = tmpfs_request(&image, &binding);
    let generation = fixture.generation.generation();
    let key = fixture
        .store
        .payload_dir(generation)
        .join(crate::host::template::KEY_NAME);
    let malformed = |acquired: &mut Acquired, what: &str| {
        let failure = fixture.plan(acquired, &request, NO_CONTENT).unwrap_err();
        assert_eq!(
            (failure.record().evidence, failure.outcome()),
            (Evidence::MalformedGeneration, Outcome::Template),
            "{what}: {failure}"
        );
        assert_untouched(acquired);
        failure
    };
    // A link in place of the key, a directory there, and a key that holds more than a key may are
    // none that the store writes: the read refuses each for what it is, a failure of the template,
    // not of an operation of the host's. Windows denies the open of a directory, as a file's access
    // control list may deny one, which is the host's, so only Unix tries the directory.
    let moved = key.with_extension("moved");
    fs::rename(&key, &moved).unwrap();
    if symlink_file(&moved, &key) {
        malformed(&mut acquired, "a link");
        fs::remove_file(&key).unwrap();
    }
    if cfg!(unix) {
        fs::create_dir(&key).unwrap();
        malformed(&mut acquired, "a directory");
        fs::remove_dir(&key).unwrap();
    }
    fs::rename(&moved, &key).unwrap();
    crate::host::store::files::write_private(&key, &vec![0; TemplateKey::MAX_LEN + 1]).unwrap();
    let failure = malformed(&mut acquired, "an oversized key");
    // So the failure quarantines the generation, once the sandbox's lock is released.
    drop(acquired);
    assert!(failure.quarantine(&fixture.store, None).unwrap());
    assert_eq!(fixture.store.tombstones().unwrap(), [generation]);
}

#[test]
fn keys_of_another_encoding_version_are_failures_of_the_host_that_quarantine_nothing() {
    // A store that a build of another encoding of keys kept, as one that a rollback of the host's
    // build leaves, holds generations whose keys this build cannot read: the host cannot restore
    // them, but a build that reads their keys can, so their failures quarantine nothing.
    let fixture = Fixture::new();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let generation = fixture.generation.generation();
    let key = fixture
        .store
        .payload_dir(generation)
        .join(crate::host::template::KEY_NAME);
    let mut bytes = fs::read(&key).unwrap();
    let version = TemplateKey::VERSION + 1;
    bytes[..2].copy_from_slice(&version.to_le_bytes());
    crate::host::store::files::write_private(&key, &bytes).unwrap();
    let failure = fixture
        .plan(&mut acquired, &tmpfs_request(&image, &binding), NO_CONTENT)
        .unwrap_err();
    assert_eq!(
        (failure.record().evidence, failure.outcome()),
        (Evidence::IncompatibleHost, Outcome::Host),
        "{failure}"
    );
    assert_eq!(failure.code(), ErrorCode::FormatUnsupported, "{failure}");
    let unread = format!(
        "the template key has encoding version {version}; this build reads version {} only",
        TemplateKey::VERSION
    );
    assert!(failure.message().contains(&unread), "{failure}");
    assert_untouched(&acquired);
    drop(acquired);
    assert!(!failure.quarantine(&fixture.store, None).unwrap());
    assert!(fixture.store.tombstones().unwrap().is_empty());
    // The generation still takes new leases.
    let next = fixture
        .store
        .acquire(
            generation,
            fixture.provision("next"),
            &files(),
            &after(&fixture.generation, 1),
            None,
        )
        .unwrap();
    assert_eq!(next.lease.generation(), generation);
}

#[test]
fn a_clone_on_a_scratch_image_claims_it_and_reads_no_content() {
    let fixture = Fixture::new();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let (scratch, scratch_binding) = fixture.scratch("scratch.ext4", SCRATCH_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let request = scratch_request(&image, &binding, &scratch, &scratch_binding);
    let plan = fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap();
    let CloneUpper::Scratch(checked) = plan.upper() else {
        panic!("the clone's upper is not on its scratch image: {plan:?}");
    };
    assert_eq!(checked.path().to_str().unwrap(), shown(&scratch));
    assert_eq!(plan.scratch_path(), Some(checked.path()));
    assert_eq!(
        checked.geometry().unwrap(),
        geometry(SCRATCH_LEN, fixture.physical)
    );
    // What the checks hold reads nothing.
    let mut byte = [0];
    assert!(checked.file().read(&mut byte).is_err());
    assert!(plan.image().file().read(&mut byte).is_err());
    // The plan holds the claim: another check finds the image claimed.
    let error = scratch_binding.check(&scratch, Instant::now()).unwrap_err();
    assert_eq!(error.code(), ErrorCode::ScratchClaimed, "{error}");
    drop(plan);
    assert!(released(|| scratch_binding
        .check(&scratch, Instant::now())
        .is_ok()));
}

#[test]
fn a_scratch_image_serves_one_clone_at_a_time() {
    let fixture = Fixture::new();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let (scratch, scratch_binding) = fixture.scratch("scratch.ext4", SCRATCH_LEN);
    let request = scratch_request(&image, &binding, &scratch, &scratch_binding);
    let mut first = fixture.acquire("first");
    let mut second = fixture.acquire("second");
    let plan = fixture.plan(&mut first, &request, NO_CONTENT).unwrap();
    // The other clone's plan waits until its deadline for the claim, then fails, holding nothing.
    let wait = Duration::from_millis(200);
    let started = Instant::now();
    let error = ClonePlan::new_with(
        &fixture.store,
        &mut second,
        &request,
        started + wait,
        NO_CONTENT,
    )
    .unwrap_err();
    assert!(started.elapsed() >= wait);
    assert_eq!(error.code(), ErrorCode::ScratchClaimed, "{error}");
    assert_eq!(error.kind(), ErrorKind::TimedOut, "{error}");
    // The image is not fresh for this clone: the request's.
    assert_eq!(error.record().evidence, Evidence::UnusableScratch);
    assert_eq!(error.outcome(), Outcome::Request);
    assert_eq!(
        error.message(),
        format!(
            "another sandbox claims the ext4 scratch image {}",
            shown(&scratch)
        )
    );
    assert_untouched(&second);
    // Once the first plan is gone, the image serves the other clone.
    drop(plan);
    let plan = fixture.plan(&mut second, &request, NO_CONTENT).unwrap();
    assert!(matches!(plan.upper(), CloneUpper::Scratch(_)));
}

#[test]
fn scratch_images_whose_identity_or_kind_changed_are_refused() {
    let fixture = Fixture::new();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let changed = |path: &Path, field: &str| {
        (
            ErrorKind::Unavailable,
            format!(
                "{} changed after its scratch binding was made: its {field} differs",
                shown(path)
            ),
        )
    };
    // Another file in the image's place, of the same length, has another identity.
    let (scratch, scratch_binding) = fixture.scratch("replaced.ext4", SCRATCH_LEN);
    let copy = fixture.path("scratches", "copy.ext4");
    fs::copy(&scratch, &copy).unwrap();
    fs::rename(&copy, &scratch).unwrap();
    let request = scratch_request(&image, &binding, &scratch, &scratch_binding);
    assert_eq!(
        refused(
            fixture.plan(&mut acquired, &request, NO_CONTENT),
            Evidence::SuppliedIdentity
        ),
        changed(&scratch, "identity")
    );
    // So has a directory in its place. The image stays, under another name, so that the
    // directory cannot take its inode number, which is its file ID on Linux.
    let (scratch, scratch_binding) = fixture.scratch("directory.ext4", SCRATCH_LEN);
    fs::rename(&scratch, fixture.path("scratches", "moved.ext4")).unwrap();
    fs::create_dir(&scratch).unwrap();
    let request = scratch_request(&image, &binding, &scratch, &scratch_binding);
    assert_eq!(
        refused(
            fixture.plan(&mut acquired, &request, NO_CONTENT),
            Evidence::SuppliedIdentity
        ),
        changed(&scratch, "identity")
    );
    // A binding of another kind names another file.
    let (scratch, scratch_binding) = fixture.scratch("kind.ext4", SCRATCH_LEN);
    let mut value = serde_json::to_value(&scratch_binding).unwrap();
    value["kind"] = "directory".into();
    let scratch_binding: ScratchBinding = serde_json::from_value(value).unwrap();
    let request = scratch_request(&image, &binding, &scratch, &scratch_binding);
    assert_eq!(
        refused(
            fixture.plan(&mut acquired, &request, NO_CONTENT),
            Evidence::SuppliedIdentity
        ),
        changed(&scratch, "kind")
    );
    assert_untouched(&acquired);
}

#[test]
fn images_no_longer_than_the_placeholder_are_served_and_longer_ones_refused() {
    let fixture = Fixture::new();
    let mut acquired = fixture.acquire("sandbox");
    let physical = fixture.physical;
    // A template serves images of either format of any length up to its placeholder's, the
    // capacity that the restore presents over each, reading zeros past a shorter one's end.
    for length in [512, IMAGE_LEN / 2, IMAGE_LEN - 512, IMAGE_LEN] {
        for gpt in [false, true] {
            let (image, binding) = if gpt {
                fixture.gpt_image("served.gpt", length)
            } else {
                fixture.image("served.erofs", length)
            };
            let plan = fixture
                .plan(&mut acquired, &tmpfs_request(&image, &binding), NO_CONTENT)
                .unwrap();
            assert_eq!(plan.image().geometry().unwrap(), geometry(length, physical));
            assert_eq!(plan.key().distro(), geometry(IMAGE_LEN, physical));
            drop(plan);
            fs::remove_file(&image).unwrap();
        }
    }
    let mut refusal = |length: u64, gpt: bool, evidence: Evidence| {
        let (image, binding) = if gpt {
            fixture.gpt_image("other.gpt", length)
        } else {
            fixture.image("other.erofs", length)
        };
        let plan = fixture.plan(&mut acquired, &tmpfs_request(&image, &binding), NO_CONTENT);
        let failed = refused(plan, evidence);
        fs::remove_file(&image).unwrap();
        (shown(&image), failed)
    };
    // A longer image, of either format, is refused.
    for (length, gpt) in [
        (IMAGE_LEN + 512, false),
        (2 * IMAGE_LEN, false),
        (IMAGE_LEN + 512, true),
    ] {
        let (image, failed) = refusal(length, gpt, Evidence::SuppliedLength);
        assert_eq!(
            failed,
            (
                ErrorKind::Unsupported,
                format!(
                    "the image {image} is {length} bytes long, longer than the template's \
                     placeholder, whose {IMAGE_LEN} bytes are the capacity of every clone's image"
                )
            )
        );
    }
    // OpenVMM attaches no block device whose length is not a nonzero multiple of 512 bytes, however
    // short.
    for length in [0, IMAGE_LEN - 1, IMAGE_LEN + 1] {
        let (image, failed) = refusal(length, false, Evidence::SuppliedGeometry);
        assert_eq!(
            failed,
            (
                ErrorKind::Unsupported,
                format!(
                    "the distro EROFS layer {image} of {length} bytes cannot be attached: OpenVMM \
                     attaches block devices of nonzero multiples of 512 bytes"
                )
            )
        );
    }
    assert_untouched(&acquired);

    // A template whose placeholder presented physical blocks of another size serves no image of
    // the filesystem of this one, whatever its length.
    let fixture = Fixture::with(|class, spec, physical| {
        let scratch = class.scratch;
        let distro = geometry(IMAGE_LEN, other_physical(physical));
        set_geometries(class, spec, distro, scratch);
    });
    let mut acquired = fixture.acquire("sandbox");
    let physical = fixture.physical;
    for length in [IMAGE_LEN / 2, IMAGE_LEN, 2 * IMAGE_LEN] {
        let (image, binding) = fixture.image("distro.erofs", length);
        let plan = fixture.plan(&mut acquired, &tmpfs_request(&image, &binding), NO_CONTENT);
        assert_eq!(
            refused(plan, Evidence::SuppliedGeometry),
            (
                ErrorKind::Unsupported,
                format!(
                    "the image {} presents logical blocks of 512 bytes and physical blocks of \
                     {physical} bytes, but the template's placeholder presented 512 and {} bytes",
                    shown(&image),
                    other_physical(physical)
                )
            )
        );
        fs::remove_file(&image).unwrap();
    }
    assert_untouched(&acquired);
}

#[test]
fn scratch_images_of_another_length_or_geometry_are_refused() {
    let fixture = Fixture::new();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    for length in [SCRATCH_LEN - 512, SCRATCH_LEN + 512] {
        let (scratch, scratch_binding) = fixture.scratch("other.ext4", length);
        let request = scratch_request(&image, &binding, &scratch, &scratch_binding);
        assert_eq!(
            refused(
                fixture.plan(&mut acquired, &request, NO_CONTENT),
                Evidence::SuppliedLength
            ),
            (
                ErrorKind::Unsupported,
                format!(
                    "the scratch image {} is {length} bytes long, not the {SCRATCH_LEN} bytes of \
                     the template's scratch",
                    shown(&scratch)
                )
            )
        );
        // The refused plan holds no claim.
        drop(
            scratch_binding
                .check(&scratch, Instant::now() + CLAIM_RELEASE_WAIT)
                .unwrap(),
        );
        fs::remove_file(&scratch).unwrap();
    }
    let (scratch, scratch_binding) = fixture.scratch("unaligned.ext4", SCRATCH_LEN + 1);
    let request = scratch_request(&image, &binding, &scratch, &scratch_binding);
    assert_eq!(
        refused(
            fixture.plan(&mut acquired, &request, NO_CONTENT),
            Evidence::SuppliedGeometry
        ),
        (
            ErrorKind::Unsupported,
            format!(
                "the ext4 scratch image {} of {} bytes cannot be attached: OpenVMM attaches block \
                 devices of nonzero multiples of 512 bytes",
                shown(&scratch),
                SCRATCH_LEN + 1
            )
        )
    );
    assert_untouched(&acquired);
}

#[test]
fn a_template_serves_clones_in_ram_whatever_geometry_its_scratch_presented() {
    // A template whose scratch presented physical blocks of another size than the filesystem of
    // the sandboxes and of the scratch images: the restore of a clone in RAM attaches the scratch
    // with no file, at the template's geometry, so the template serves clones in RAM there, while
    // a scratch image must present that geometry, which no file of that filesystem does.
    let fixture = Fixture::with(|class, spec, physical| {
        let distro = class.distro;
        let scratch = geometry(SCRATCH_LEN, other_physical(physical));
        set_geometries(class, spec, distro, scratch);
    });
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let (physical, other) = (fixture.physical, other_physical(fixture.physical));
    let plan = fixture
        .plan(&mut acquired, &tmpfs_request(&image, &binding), NO_CONTENT)
        .unwrap();
    assert_eq!(plan.scratch_path(), None);
    assert_eq!(plan.key().scratch(), geometry(SCRATCH_LEN, other));
    drop(plan);
    let (scratch, scratch_binding) = fixture.scratch("scratch.ext4", SCRATCH_LEN);
    let request = scratch_request(&image, &binding, &scratch, &scratch_binding);
    assert_eq!(
        refused(
            fixture.plan(&mut acquired, &request, NO_CONTENT),
            Evidence::SuppliedGeometry
        ),
        (
            ErrorKind::Unsupported,
            format!(
                "the scratch image {} presents logical blocks of 512 bytes and physical blocks of \
                 {physical} bytes, but the template's scratch presented 512 and {other} bytes",
                shown(&scratch)
            )
        )
    );
    assert_untouched(&acquired);
}

/// Files on hugetlbfs, whose I/O block size is a huge page, present physical blocks of 2 MiB,
/// unlike the template's: an image, as long as the placeholder or shorter, and a scratch image,
/// which each reach such a file.
#[cfg(target_os = "linux")]
#[test]
fn files_on_a_filesystem_of_another_block_size_are_refused() {
    use crate::host::sandbox::distro::import_with;
    use crate::host::sandbox::scratch::bind_with;

    const HUGE_PAGE: u64 = 2 * MIB;

    let fixture = Fixture::new();
    if u64::from(fixture.physical) == HUGE_PAGE {
        eprintln!("the files of the test's directory present physical blocks of a huge page");
        return;
    }
    let Some(huge_images) = [IMAGE_LEN, HUGE_PAGE]
        .into_iter()
        .map(huge_file)
        .collect::<Option<Vec<File>>>()
    else {
        return;
    };
    let mut acquired = fixture.acquire("sandbox");
    let presented = |what: String, device: &str| {
        (
            ErrorKind::Unsupported,
            format!(
                "{what} presents logical blocks of 512 bytes and physical blocks of {HUGE_PAGE} \
                 bytes, but the template's {device} presented 512 and {} bytes",
                fixture.physical
            ),
        )
    };
    // The image, as long as the placeholder or shorter: its binding and the check of the binding
    // reach a file on hugetlbfs at its path.
    let image = fixture.path("images", "huge.erofs");
    for huge_image in &huge_images {
        let open = |path: &Path| {
            if path == image {
                huge_image.try_clone()
            } else {
                Err(io::Error::from(io::ErrorKind::NotFound))
            }
        };
        let huge_distro = distro::Access {
            open: &open,
            seal: &FileSeal::of,
        };
        let binding = import_with(&layer(&image), PolicyVerdict::ACCEPTED, huge_distro).unwrap();
        let access = Access {
            distro: huge_distro,
            ..NO_CONTENT
        };
        let plan = fixture.plan(&mut acquired, &tmpfs_request(&image, &binding), access);
        assert_eq!(
            refused(plan, Evidence::SuppliedGeometry),
            presented(format!("the image {}", shown(&image)), "placeholder")
        );
    }
    // The scratch image: its binding and its check reach a file on hugetlbfs at its path.
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let huge_scratch = huge_file(SCRATCH_LEN).unwrap();
    let scratch = fixture.path("scratches", "huge.ext4");
    let open = |_: &Path| huge_scratch.try_clone();
    let scratch_access = scratch::Access { open: &open };
    let scratch_binding = bind_with(&paths::normalized(&scratch), scratch_access).unwrap();
    let access = Access {
        scratch: scratch_access,
        ..NO_CONTENT
    };
    let request = scratch_request(&image, &binding, &scratch, &scratch_binding);
    assert_eq!(
        refused(
            fixture.plan(&mut acquired, &request, access),
            Evidence::SuppliedGeometry
        ),
        presented(format!("the scratch image {}", shown(&scratch)), "scratch")
    );
    assert_untouched(&acquired);
}

#[test]
fn memory_targets_are_whole_blocks_from_the_templates_ram_to_its_capacity() {
    let fixture = Fixture::new();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let block = MEMORY_BLOCK_SIZE;
    let generation = fixture.generation.generation();
    for memory in [
        0,
        MEMORY - block,
        MEMORY + 1,
        MEMORY + block / 2,
        MEMORY_CAPACITY - 4096,
        MEMORY_CAPACITY + block,
        u64::MAX,
    ] {
        let request = CloneRequest {
            memory,
            ..tmpfs_request(&image, &binding)
        };
        assert_eq!(
            refused(
                fixture.plan(&mut acquired, &request, NO_CONTENT),
                Evidence::InvalidTargets
            ),
            (
                ErrorKind::Unsupported,
                format!(
                    "a clone of the template's generation {generation} cannot restore with \
                     {memory} bytes of RAM: its memory target must be whole blocks of 128 MiB, \
                     and of the template's memory blocks of {block} bytes, from the template's \
                     {MEMORY} bytes of RAM to its capacity of {MEMORY_CAPACITY} bytes"
                )
            )
        );
    }
    assert_untouched(&acquired);
    for memory in [MEMORY, MEMORY + block, MEMORY_CAPACITY] {
        let request = CloneRequest {
            memory,
            ..tmpfs_request(&image, &binding)
        };
        let plan = fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap();
        assert_eq!(plan.memory(), memory);
    }
}

#[test]
fn processor_targets_are_counts_from_the_templates_processors_online_at_boot_to_its_capacity() {
    let refusal = |generation: &str, processors: u32, boot: u32, capacity: u32| {
        (
            ErrorKind::Unsupported,
            format!(
                "a clone of the template's generation {generation} cannot restore with a \
                 processor target of {processors}: a target must be 1, 2, 4, or 8, from the \
                 template's processors online at boot, {boot}, to its capacity, {capacity}"
            ),
        )
    };
    // A template that boots with 2 of its 8 processors online serves each supported count from 2
    // to 8.
    let fixture = Fixture::with(|class, spec, _| set_processors(class, spec, 8, 2));
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let generation = fixture.generation.generation().to_string();
    for processors in [0, 1, 3, 6, 16, u32::MAX] {
        let request = CloneRequest {
            processors,
            ..tmpfs_request(&image, &binding)
        };
        assert_eq!(
            refused(
                fixture.plan(&mut acquired, &request, NO_CONTENT),
                Evidence::InvalidTargets
            ),
            refusal(&generation, processors, 2, 8)
        );
    }
    assert_untouched(&acquired);
    for processors in [2, 4, 8] {
        let request = CloneRequest {
            processors,
            ..tmpfs_request(&image, &binding)
        };
        let plan = fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap();
        assert_eq!(plan.processors(), processors);
        assert_eq!(plan.activation().configuration().resources.cpus, processors);
    }
    // A template whose processors are all online at boot records none online at boot, and serves
    // its capacity alone.
    let fixture = Fixture::new();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let generation = fixture.generation.generation().to_string();
    for processors in [1, 4] {
        let request = CloneRequest {
            processors,
            ..tmpfs_request(&image, &binding)
        };
        assert_eq!(
            refused(
                fixture.plan(&mut acquired, &request, NO_CONTENT),
                Evidence::InvalidTargets
            ),
            refusal(&generation, processors, PROCESSORS, PROCESSORS)
        );
    }
    let plan = fixture
        .plan(&mut acquired, &tmpfs_request(&image, &binding), NO_CONTENT)
        .unwrap();
    assert_eq!(plan.processors(), PROCESSORS);
}

#[test]
fn requests_that_no_clone_takes_are_refused_before_anything_is_opened() {
    let fixture = Fixture::new();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let (scratch, scratch_binding) = fixture.scratch("scratch.ext4", SCRATCH_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let mut refusal = |request: &CloneRequest, evidence: Evidence| {
        let (kind, message) = refused(fixture.plan(&mut acquired, request, NO_CONTENT), evidence);
        assert_eq!(kind, ErrorKind::Unsupported, "{message}");
        message
    };
    let tmpfs = tmpfs_request(&image, &binding);
    let distro = tmpfs.adapter.distro().clone();
    // A plan of the sandbox profile.
    let profile = CloneRequest {
        adapter: SandboxPlan::new(
            distro.clone(),
            SandboxUpper::RAM,
            SandboxSettings::default(),
        )
        .unwrap(),
        ..tmpfs.clone()
    };
    let message = refusal(&profile, Evidence::InvalidSettings);
    assert!(
        message.starts_with("a clone restores an adapter's plan, not a plan of the sandbox"),
        "{message}"
    );
    // An image that is not the one that the binding names.
    let other = Uuid::parse("22222222-2222-2222-2222-222222222222").unwrap();
    let mistaken = CloneRequest {
        adapter: SandboxPlan::adapter(
            SandboxDistro::new(image.to_str().unwrap(), RootImage::erofs(Some(other))),
            SandboxUpper::RAM,
        ),
        ..tmpfs.clone()
    };
    assert_eq!(
        refusal(&mistaken, Evidence::InvalidFormat),
        format!(
            "the image {} is not the erofs layer with identity {UUID} that its distro binding \
             names",
            shown(&image)
        )
    );
    // An image of another format than the one that the binding names, either way.
    let identity = Some(Uuid::parse(UUID).unwrap());
    let as_gpt = CloneRequest {
        adapter: SandboxPlan::adapter(
            SandboxDistro::new(image.to_str().unwrap(), RootImage::gpt(identity)),
            SandboxUpper::RAM,
        ),
        ..tmpfs.clone()
    };
    assert_eq!(
        refusal(&as_gpt, Evidence::InvalidFormat),
        format!(
            "the image {} is not the erofs layer with identity {UUID} that its distro binding \
             names",
            shown(&image)
        )
    );
    let (gpt, gpt_binding) = fixture.gpt_image("distro.gpt", IMAGE_LEN);
    let as_erofs = CloneRequest {
        adapter: SandboxPlan::adapter(
            SandboxDistro::new(gpt.to_str().unwrap(), RootImage::erofs(identity)),
            SandboxUpper::RAM,
        ),
        binding: gpt_binding,
        ..tmpfs.clone()
    };
    assert_eq!(
        refusal(&as_erofs, Evidence::InvalidFormat),
        format!(
            "the image {} is not the gpt layer with identity {UUID} that its distro binding names",
            shown(&gpt)
        )
    );
    // Paths that OpenVMM would not open as the plan checks them.
    let at = |path: &str| CloneRequest {
        adapter: SandboxPlan::adapter(SandboxDistro::new(path, distro.image()), SandboxUpper::RAM),
        ..tmpfs.clone()
    };
    let message = refusal(&at("distro.erofs"), Evidence::InvalidSettings);
    assert!(
        message.starts_with("the image of a clone must be absolute"),
        "{message}"
    );
    let comma = at(&format!("{},x", image.to_str().unwrap()));
    let message = refusal(&comma, Evidence::InvalidSettings);
    assert!(
        message.starts_with("disk paths containing commas, semicolons, or unbalanced brackets"),
        "{message}"
    );
    let vhdx = at(image.with_extension("vhdx").to_str().unwrap());
    let message = refusal(&vhdx, Evidence::InvalidSettings);
    assert!(
        message.starts_with("disk paths ending in .vhdx"),
        "{message}"
    );
    let on_scratch = |path: &str| CloneRequest {
        adapter: SandboxPlan::adapter(distro.clone(), SandboxUpper::scratch(path)),
        scratch: Some(scratch_binding.clone()),
        ..tmpfs.clone()
    };
    let message = refusal(&on_scratch("scratch.ext4"), Evidence::InvalidSettings);
    assert!(
        message.starts_with("the scratch image of a clone must be absolute"),
        "{message}"
    );
    let iso = on_scratch(scratch.with_extension("iso").to_str().unwrap());
    let message = refusal(&iso, Evidence::InvalidSettings);
    assert!(
        message.starts_with("disk paths ending in .iso"),
        "{message}"
    );
    // An upper on a scratch image needs its binding, and one in RAM takes none.
    let unbound = CloneRequest {
        scratch: None,
        ..scratch_request(&image, &binding, &scratch, &scratch_binding)
    };
    assert_eq!(
        refusal(&unbound, Evidence::InvalidSettings),
        format!(
            "the upper of a clone on the scratch image {} needs the image's scratch binding",
            shown(&scratch)
        )
    );
    let bound = CloneRequest {
        scratch: Some(scratch_binding.clone()),
        ..tmpfs.clone()
    };
    assert_eq!(
        refusal(&bound, Evidence::InvalidSettings),
        "the upper of a clone in RAM takes no scratch binding"
    );
    // None of them opened anything: the scratch image is free, and the lease still reserves the
    // clone's launch, which a plan of a request that a clone takes then serves.
    drop(scratch_binding.check(&scratch, Instant::now()).unwrap());
    assert_untouched(&acquired);
    drop(fixture.plan(&mut acquired, &tmpfs, NO_CONTENT).unwrap());
}

#[test]
fn templates_serve_the_networks_of_every_policy_at_their_nics_address_alone() {
    let fixture = Fixture::new();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let request = tmpfs_request(&image, &binding);
    let generation = fixture.generation.generation();
    let attached = with_network(request.clone(), Some(network(NetworkAccess::Allow)));
    assert_eq!(
        refused(
            fixture.plan(&mut acquired, &attached, NO_CONTENT),
            Evidence::InvalidSettings
        ),
        (
            ErrorKind::Unsupported,
            format!(
                "the adapter's plan of a clone attaches a network, but the template's generation \
                 {generation} has no NIC"
            )
        )
    );
    assert_untouched(&acquired);
    // A network that lets nothing through attaches no NIC.
    let closed = with_network(request, Some(network(NetworkAccess::Deny)));
    drop(fixture.plan(&mut acquired, &closed, NO_CONTENT).unwrap());

    // A template with a NIC serves no clone without a network.
    let fixture = network_fixture();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let request = tmpfs_request(&image, &binding);
    let generation = fixture.generation.generation();
    let closed = with_network(request.clone(), Some(network(NetworkAccess::Deny)));
    for request in [&request, &closed] {
        assert_eq!(
            refused(
                fixture.plan(&mut acquired, request, NO_CONTENT),
                Evidence::InvalidSettings
            ),
            (
                ErrorKind::Unsupported,
                format!(
                    "the template's generation {generation} has a NIC, which its clones restore \
                     with the network of the adapter's plan, but the plan has none"
                )
            )
        );
        assert_untouched(&acquired);
    }
    // Nor the clones of a network at another address.
    for (other, policy) in [
        ("10.0.0.3/24", NetworkPolicy::new(NetworkAccess::Allow)),
        ("10.0.0.2/16", NetworkPolicy::new(NetworkAccess::Allow)),
    ] {
        let other = SandboxNetwork::new(other, policy).unwrap();
        let address = other.guest_network().to_owned();
        let other = with_network(request.clone(), Some(other));
        assert_eq!(
            refused(
                fixture.plan(&mut acquired, &other, NO_CONTENT),
                Evidence::InvalidSettings
            ),
            (
                ErrorKind::Unsupported,
                format!(
                    "the adapter's plan of a clone attaches a network at {address}, but the NIC \
                     of the template's generation {generation} is at 10.0.0.2/24: a template \
                     serves the clones of its own guest's address alone"
                )
            )
        );
        assert_untouched(&acquired);
    }
    // It serves the clones of every policy at its guest's address, whatever the policy of its
    // class's network, since each clone's restore replaces the complete deny-all policy of the
    // template's NIC with its own.
    for policy in attaching() {
        let attached = SandboxNetwork::new("10.0.0.2/24", policy).unwrap();
        let attached = with_network(request.clone(), Some(attached));
        drop(fixture.plan(&mut acquired, &attached, NO_CONTENT).unwrap());
    }
}

#[test]
fn clones_are_planned_under_the_lock_that_acquired_their_lease_before_their_launch() {
    let fixture = Fixture::new();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let request = tmpfs_request(&image, &binding);
    // A lease of another store.
    let other = Fixture::new();
    let mut elsewhere = other.acquire("sandbox");
    let plan = fixture.plan(&mut elsewhere, &request, NO_CONTENT);
    let (kind, message) = refused(plan, Evidence::HostFailure);
    assert_eq!(kind, ErrorKind::Other, "{message}");
    assert_eq!(
        message,
        format!(
            "the lease {} is one of the template store {}, not of this one, {}",
            elsewhere.lease.id(),
            other.store.id(),
            fixture.store.id()
        )
    );
    assert_untouched(&elsewhere);
    // Another lock of the sandbox, which reserves no launch, as one taken once the lock that
    // acquired the lease is gone.
    let Acquired { lease, locked } = fixture.acquire("sandbox");
    drop(locked);
    let locked = SandboxStore::lock_existing(lease.sandbox(), &files(), None).unwrap();
    let mut relocked = Acquired { lease, locked };
    let plan = fixture.plan(&mut relocked, &request, NO_CONTENT);
    let (kind, message) = refused(plan, Evidence::HostFailure);
    assert_eq!(kind, ErrorKind::Other, "{message}");
    assert!(
        message.starts_with("a clone is planned under the lock that acquired its lease"),
        "{message}"
    );
    // The lock that acquired a lease, once the launch that the lease reserves has begun.
    let mut acquired = fixture.acquire("begun");
    let key = fixture.store.read_key(&fixture.generation).unwrap();
    drop(
        acquired
            .locked
            .begin_launch(acquired.lease.launch(), key.runtime(), &[7; CAPABILITY_LEN])
            .unwrap(),
    );
    let plan = fixture.plan(&mut acquired, &request, NO_CONTENT);
    let (kind, message) = refused(plan, Evidence::HostFailure);
    assert_eq!(kind, ErrorKind::Other, "{message}");
}

#[test]
fn files_that_their_paths_do_not_name_as_checked_are_refused() {
    let fixture = Fixture::new();
    let mut acquired = fixture.acquire("sandbox");
    // An image whose path is a link, which OpenVMM's open does not follow.
    let (target, _) = fixture.image("target.erofs", IMAGE_LEN);
    let link = fixture.path("images", "link.erofs");
    if symlink_file(&target, &link) {
        let binding = DistroBinding::import(&layer(&link), PolicyVerdict::ACCEPTED).unwrap();
        let plan = fixture.plan(&mut acquired, &tmpfs_request(&link, &binding), NO_CONTENT);
        assert_eq!(
            refused(plan, Evidence::SuppliedIdentity),
            (
                ErrorKind::Unavailable,
                format!(
                    "the image {} cannot be used: it is a link, or not a regular file",
                    shown(&link)
                )
            )
        );
        // A scratch image whose path is a link.
        let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
        let (target, _) = fixture.scratch("target.ext4", SCRATCH_LEN);
        let link = fixture.path("scratches", "link.ext4");
        assert!(symlink_file(&target, &link));
        let scratch_binding = ScratchBinding::bind(&link).unwrap();
        let request = scratch_request(&image, &binding, &link, &scratch_binding);
        assert_eq!(
            refused(
                fixture.plan(&mut acquired, &request, NO_CONTENT),
                Evidence::SuppliedIdentity
            ),
            (
                ErrorKind::Unavailable,
                format!(
                    "the scratch image {} cannot be used: it is a link, or not a regular file",
                    shown(&link)
                )
            )
        );
        // The refused plan holds no claim of the image.
        drop(
            scratch_binding
                .check(&link, Instant::now() + CLAIM_RELEASE_WAIT)
                .unwrap(),
        );
    }
    // An image that others may change.
    let path = fixture.path("images", "shared.erofs");
    File::create(&path).unwrap().set_len(IMAGE_LEN).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
    }
    #[cfg(windows)]
    platform::set_access(&path, "D:P(A;;FA;;;OW)(A;;FA;;;SY)(A;;FA;;;WD)").unwrap();
    let binding = DistroBinding::import(&layer(&path), PolicyVerdict::ACCEPTED).unwrap();
    let problem = if cfg!(windows) {
        "allows other users to change it"
    } else {
        "is writable by other users (mode 666)"
    };
    let plan = fixture.plan(&mut acquired, &tmpfs_request(&path, &binding), NO_CONTENT);
    assert_eq!(
        refused(plan, Evidence::SuppliedIdentity),
        (
            ErrorKind::Unavailable,
            format!("the image {} {problem}", shown(&path))
        )
    );
    assert_untouched(&acquired);
}

/// Returns the SHA-256 digest of `domain` and of the serde form of `binding`, as the identity of a
/// binding is documented.
fn digest_of(domain: &[u8], binding: &impl serde::Serialize) -> Identity {
    let digest = Sha256::new()
        .chain_update(domain)
        .chain_update(serde_json::to_vec(binding).unwrap())
        .finalize();
    Identity::from_bytes(digest.into())
}

#[test]
fn the_activation_record_of_a_clone_in_ram_names_its_restore_image_and_settings() {
    let fixture = Fixture::new();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let request = tmpfs_request(&image, &binding);
    let plan = fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap();
    let record = plan.activation();
    let restore = record.restore().unwrap();
    assert_eq!(restore.template, acquired.lease.template());
    assert_eq!(
        record.configuration(),
        &Configuration {
            image: Image::Erofs {
                binding: digest_of(b"nvx distro binding\0", &binding),
                uuid: Some(Uuid::parse(UUID).unwrap()),
            },
            // A clone in RAM has no journal policy, whatever the request's.
            upper: Upper::Tmpfs,
            overlay: OverlayMode::UntrustedLowers,
            export: None,
            mappings: 0,
            network: None,
            workload: workload(),
            resources: ExpectedResources {
                cpus: PROCESSORS,
                memory: MEMORY,
            },
        }
    );
    // The plan shows the record, which holds nothing secret.
    let shown_plan = format!("{plan:?}");
    assert!(
        shown_plan.contains(&format!("activation: {record:?}")),
        "{shown_plan}"
    );
    // Each plan draws its own transaction.
    let transaction = restore.transaction;
    drop(plan);
    let again = fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap();
    let restored = again.activation().restore().unwrap();
    assert_ne!(restored.transaction, transaction);
    assert_eq!(restored.template, acquired.lease.template());
    // A larger memory target is the resources that the clone must have.
    drop(again);
    let larger = CloneRequest {
        memory: 2 * MEMORY,
        ..request
    };
    let plan = fixture.plan(&mut acquired, &larger, NO_CONTENT).unwrap();
    assert_eq!(
        plan.activation().configuration().resources.memory,
        2 * MEMORY
    );
}

#[test]
fn the_activation_record_of_a_clone_on_a_scratch_image_names_its_binding_and_journal() {
    let fixture = Fixture::new();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let (scratch, scratch_binding) = fixture.scratch("scratch.ext4", SCRATCH_LEN);
    let mut acquired = fixture.acquire("sandbox");
    for journal in [Journal::Forbidden, Journal::Allowed] {
        let request = CloneRequest {
            overlay: OverlayMode::TrustedLowers,
            journal,
            ..scratch_request(&image, &binding, &scratch, &scratch_binding)
        };
        let plan = fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap();
        let configuration = plan.activation().configuration();
        assert_eq!(
            configuration.upper,
            Upper::Scratch {
                binding: digest_of(b"nvx scratch binding\0", &scratch_binding),
                journal,
            }
        );
        assert_eq!(configuration.overlay, OverlayMode::TrustedLowers);
        assert_eq!(
            *configuration.image.binding(),
            digest_of(b"nvx distro binding\0", &binding)
        );
    }
    // The identities of two bindings differ, as their files do.
    let (other, other_binding) = fixture.scratch("other.ext4", SCRATCH_LEN);
    let request = scratch_request(&image, &binding, &other, &other_binding);
    let plan = fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap();
    assert_ne!(
        plan.activation().configuration().upper,
        Upper::Scratch {
            binding: digest_of(b"nvx scratch binding\0", &scratch_binding),
            journal: Journal::Forbidden,
        }
    );
}

#[test]
fn the_activation_record_of_a_gpt_clone_names_its_disk_and_the_bound_of_its_first_partition() {
    let fixture = Fixture::new();
    let (image, binding) = fixture.gpt_image("distro.gpt", IMAGE_LEN);
    let (scratch, scratch_binding) = fixture.scratch("scratch.ext4", SCRATCH_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let gpt = |first_partition_bound| Image::Gpt {
        binding: digest_of(b"nvx distro binding\0", &binding),
        disk_guid: Some(Uuid::parse(UUID).unwrap()),
        first_partition_bound,
    };
    // A GPT disk of the placeholder's geometry serves over RAM and over a scratch image, as an
    // EROFS layer does, and its plan reads none of its content: the guest reads and checks the
    // disk's partition table as it assembles the root.
    for request in [
        tmpfs_request(&image, &binding),
        scratch_request(&image, &binding, &scratch, &scratch_binding),
    ] {
        let plan = fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap();
        assert_eq!(plan.image().path().to_str().unwrap(), shown(&image));
        assert_eq!(
            plan.image().geometry().unwrap(),
            geometry(IMAGE_LEN, fixture.physical)
        );
        let mut byte = [0];
        assert!(plan.image().file().read(&mut byte).is_err());
        let configuration = plan.activation().configuration();
        assert_eq!(configuration.image, gpt(FIRST_PARTITION_BOUND));
        assert_eq!(configuration.workload, workload());
    }
    // The record keeps the request's bound, whatever it is from 1 on.
    for bound in [1, u64::MAX] {
        let request = CloneRequest {
            first_partition_bound: bound,
            ..tmpfs_request(&image, &binding)
        };
        let plan = fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap();
        assert_eq!(plan.activation().configuration().image, gpt(bound));
    }
}

#[test]
fn the_activation_record_of_a_clone_of_a_template_with_a_nic_names_its_own_network() {
    let fixture = network_fixture();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let gateway = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
    // The template's NIC has the IPv6 gateway fd00::a00:1.
    let ipv6_gateway = IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0xa00, 1));
    let mut ipv6_dns = NetworkPolicy::new(NetworkAccess::Deny);
    for protocol in [Protocol::Tcp, Protocol::Udp] {
        ipv6_dns = ipv6_dns.with_allow(EgressRule::to("fd00::a00:1/128").on_port(protocol, 53));
    }
    let mut outbound = NetworkPolicy::new(NetworkAccess::Allow);
    outbound.host_loopback = HostLoopback::Outbound;
    let mut policies = vec![ipv6_dns, outbound];
    policies.extend(attaching());
    let (unpublished, published) = (Publication::Unpublished, Publication::Published);
    let expected = [
        (Some(ipv6_gateway), unpublished, None),
        (Some(gateway), unpublished, None),
        (Some(gateway), unpublished, None),
        (None, unpublished, None),
        (None, unpublished, Some("http://10.0.0.1:3128")),
        (None, published, None),
        (Some(gateway), published, None),
    ];
    assert_eq!(policies.len(), expected.len());
    // Each clone's record names its own network, whatever the policy of the template's class:
    // the identity of the network, the DNS server that OpenVMM serves the guest at a gateway
    // under its policy, whether the guest publishes its listeners, and the proxy's URL as the
    // guest sees it, at its gateway.
    let mut identities = HashSet::new();
    for (policy, (dns, publication, proxy)) in policies.into_iter().zip(expected) {
        let network = SandboxNetwork::new("10.0.0.2/24", policy).unwrap();
        let request = with_network(tmpfs_request(&image, &binding), Some(network.clone()));
        let plan = fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap();
        let recorded = plan.activation().configuration().network.clone();
        assert_eq!(
            recorded,
            Some(Network {
                policy: digest_of(b"nvx network policy\0", &network),
                dns,
                publication,
                proxy: proxy.map(str::to_owned),
            }),
            "{network:?}"
        );
        assert!(identities.insert(recorded.unwrap().policy));
    }
}

#[test]
fn settings_that_no_activation_record_holds_are_refused_before_anything_is_created() {
    let fixture = Fixture::new();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let request = tmpfs_request(&image, &binding);
    let with_workload = |workload: WorkloadView| CloneRequest {
        workload,
        ..request.clone()
    };
    for (workload, problem) in [
        (
            WorkloadView {
                hostname: Vec::new(),
                ..workload()
            },
            "the hostname is not 1 to 64 bytes without a NUL",
        ),
        (
            WorkloadView {
                hostname: vec![b'h'; 65],
                ..workload()
            },
            "the hostname is not 1 to 64 bytes without a NUL",
        ),
        (
            WorkloadView {
                hostname: b"clone\0".to_vec(),
                ..workload()
            },
            "the hostname is not 1 to 64 bytes without a NUL",
        ),
        (
            WorkloadView {
                identity: Some(WorkloadIdentity { uid: 0, gid: 1000 }),
                ..workload()
            },
            "the workload identity's UID or GID is 0 or 4294967295",
        ),
    ] {
        let plan = fixture.plan(&mut acquired, &with_workload(workload), NO_CONTENT);
        assert_eq!(
            refused(plan, Evidence::InvalidActivate),
            (
                ErrorKind::Unsupported,
                format!("no clone activates with the settings of the request: {problem}")
            )
        );
        assert_untouched(&acquired);
    }
    // A GPT disk's first-partition bound of 0, which an EROFS layer's record leaves out.
    let (path, gpt_binding) = fixture.gpt_image("distro.gpt", IMAGE_LEN);
    let unbounded = CloneRequest {
        first_partition_bound: 0,
        ..tmpfs_request(&path, &gpt_binding)
    };
    let plan = fixture.plan(&mut acquired, &unbounded, NO_CONTENT);
    assert_eq!(
        refused(plan, Evidence::InvalidActivate),
        (
            ErrorKind::Unsupported,
            "no clone activates with the settings of the request: the bound of a GPT disk's first \
             partition is 0"
                .to_owned()
        )
    );
    assert_untouched(&acquired);
    let erofs = CloneRequest {
        first_partition_bound: 0,
        ..request.clone()
    };
    drop(fixture.plan(&mut acquired, &erofs, NO_CONTENT).unwrap());
    drop(fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap());
}

#[test]
fn only_a_path_that_no_longer_names_its_supplied_file_is_the_request_s() {
    // The open's refusal of a link or of a file that is not regular, which the system does not
    // report, a file that went, and a path that the current user may no longer open, are the
    // path's.
    assert!(path_refused(&io::Error::other(
        "it is a link, or not a regular file"
    )));
    #[cfg(unix)]
    let (refusals, failures) = {
        use nix::errno::Errno;

        (
            [Errno::ENOENT, Errno::ENOTDIR, Errno::EACCES],
            [Errno::EIO, Errno::EMFILE, Errno::ENOMEM],
        )
    };
    // ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, and ERROR_ACCESS_DENIED; ERROR_IO_DEVICE,
    // ERROR_NOT_ENOUGH_MEMORY, and ERROR_NO_SYSTEM_RESOURCES.
    #[cfg(windows)]
    let (refusals, failures) = ([2_u32, 3, 5], [1117_u32, 8, 1450]);
    for errno in refusals {
        let error = io::Error::from_raw_os_error(errno as i32);
        assert!(path_refused(&error), "{error}");
    }
    for errno in failures {
        let error = io::Error::from_raw_os_error(errno as i32);
        assert!(!path_refused(&error), "{error}");
    }
    // A file that went from its path is the request's.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("image");
    let fault = check_at_path(&path, (1, 2), "the image").unwrap_err();
    assert_eq!(fault.evidence, Evidence::SuppliedIdentity);
    assert_eq!(
        fault.error.kind(),
        ErrorKind::Unavailable,
        "{}",
        fault.error
    );
    assert!(
        fault
            .error
            .message()
            .starts_with("the image cannot be used: "),
        "{}",
        fault.error
    );
}

#[test]
fn a_clone_with_an_export_records_its_plan_and_maps_from_its_children() {
    let fixture = Fixture::new();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    // An aggregate of directories and files, read-only and read-write, with hidden and re-exposed
    // paths, as a cold launch exports one.
    let (export, _) = fixture.aggregate_export("aggregate");
    assert_eq!(export.children.len(), 4);
    let request = with_export(tmpfs_request(&image, &binding), &export);
    let plan = fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap();
    assert_eq!(plan.export(), Some(&export));
    // The record names the export, an aggregate, by the identity of its plan, and announces its
    // mappings, the plan's binds.
    let configuration = plan.activation().configuration();
    assert_eq!(
        configuration.export,
        Some(Export {
            kind: ExportKind::Aggregate,
            plan: digest_of(b"nvx export plan\0", &export),
        })
    );
    assert_eq!(configuration.mappings, export.binds.len());
    // The table is a cold launch's: its sources name each path by its child's index, as the
    // aggregate's root lists the children.
    assert_eq!(plan.table(), export.guest_table());
    let sources: Vec<String> = plan
        .table()
        .into_iter()
        .map(|mapping| mapping.source)
        .collect();
    assert_eq!(sources, ["0", "1", "2", "3", "0/work"]);
    // The identity covers the whole plan: another aggregate's is another.
    drop(plan);
    let other = fixture.data_export("other");
    let request = with_export(tmpfs_request(&image, &binding), &other);
    let plan = fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap();
    let recorded = plan.activation().configuration().export.unwrap();
    assert_eq!(recorded.kind, ExportKind::Aggregate);
    assert_eq!(recorded.plan, digest_of(b"nvx export plan\0", &other));
    assert_ne!(recorded.plan, digest_of(b"nvx export plan\0", &export));
    assert_eq!(plan.table(), data_table());
    // A clone without mappings has no export, and its record none, so its template's slot stays
    // dormant.
    drop(plan);
    let request = tmpfs_request(&image, &binding);
    let plan = fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap();
    assert_eq!(plan.export(), None);
    assert!(plan.table().is_empty());
    let configuration = plan.activation().configuration();
    assert_eq!((&configuration.export, configuration.mappings), (&None, 0));
}

#[test]
fn exports_that_no_cold_launch_attaches_are_refused_before_anything_is_opened() {
    let fixture = Fixture::new();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let request = tmpfs_request(&image, &binding);
    let mut refusal = |export: &ExportPlan| {
        let request = with_export(request.clone(), export);
        let plan = fixture.plan(&mut acquired, &request, NO_CONTENT);
        let (kind, message) = refused(plan, Evidence::InvalidMappings);
        assert_eq!(kind, ErrorKind::Unsupported, "{message}");
        message
    };
    // Mappings without an export, an export without mappings, and an export of more directories
    // and files than OpenVMM attaches.
    let export = fixture.data_export("data");
    let unexported = ExportPlan {
        children: Vec::new(),
        ..export.clone()
    };
    assert_eq!(
        refusal(&unexported),
        "the recorded export plan is invalid: it exports 0 directories and files, but OpenVMM \
         exports from 1 to 256"
    );
    let unmapped = ExportPlan {
        binds: Vec::new(),
        ..export.clone()
    };
    assert_eq!(
        refusal(&unmapped),
        "the recorded export plan is invalid: it maps 0 paths, but a sandbox maps from 1 to 4096"
    );
    let crowded = ExportPlan {
        children: vec![export.children[0].clone(); filesystem::MAX_CHILDREN + 1],
        ..export.clone()
    };
    assert_eq!(
        refusal(&crowded),
        "the recorded export plan is invalid: it exports 257 directories and files, but OpenVMM \
         exports from 1 to 256"
    );
    // A plan that maps a target that the container root keeps for itself.
    let mut reserved = export.clone();
    reserved.binds[1].target = "/proc/work".to_owned();
    let message = refusal(&reserved);
    assert!(
        message.starts_with("the recorded export plan is invalid"),
        "{message}"
    );
    // None of them opened anything, and an aggregate of directories and files serves a clone, as
    // the export of one directory does.
    assert_untouched(&acquired);
    let (aggregate, _) = fixture.aggregate_export("aggregate");
    for export in [aggregate, export] {
        let request = with_export(request.clone(), &export);
        drop(fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap());
    }
}

#[test]
fn exports_that_overlap_the_template_store_are_refused_before_anything_is_opened() {
    // The plan compares the paths that the export's plan resolved with the store's, resolved as
    // well: this store's path names its directory in another spelling than its resolved one.
    let fixture = Fixture::in_store(|directory| alias(directory).join("templates"), |_, _, _| {});
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let request = tmpfs_request(&image, &binding);
    let store = filesystem::canonicalize(fixture.store.dir()).unwrap();
    assert_ne!(store, fixture.store.dir());
    let payload = fixture.store.payload_dir(fixture.generation.generation());
    // The store's directory, the payload of a generation inside it, and a directory that holds
    // the store, which a guest could reach the payload through, and write it if writable.
    for (dir, read_only) in [
        (store.clone(), true),
        (payload.clone(), false),
        (fixture.directory.path().to_owned(), true),
    ] {
        let export = export_plan(&[(&dir, "/data", read_only)], &[], &[]);
        let request = with_export(request.clone(), &export);
        let plan = fixture.plan(&mut acquired, &request, NO_CONTENT);
        assert_eq!(
            refused(plan, Evidence::InvalidMappings),
            (
                ErrorKind::Unsupported,
                format!(
                    "the export of a clone cannot share {}, which overlaps the template store {}, \
                     whose leased payloads nothing may change",
                    export.children[0].root.display(),
                    store.display()
                )
            )
        );
        assert_untouched(&acquired);
    }
    // Every directory and file of an aggregate is checked: a file of the store's, beside a
    // directory that is no part of it.
    let beside = fixture.exported("beside", &[]);
    let key = filesystem::canonicalize(&payload.join(KEY_NAME)).unwrap();
    let export = export_plan(
        &[(&beside, "/beside", false), (&key, "/key.bin", true)],
        &[],
        &[],
    );
    assert_eq!(export.children.len(), 2);
    let request = with_export(request, &export);
    let plan = fixture.plan(&mut acquired, &request, NO_CONTENT);
    assert_eq!(
        refused(plan, Evidence::InvalidMappings),
        (
            ErrorKind::Unsupported,
            format!(
                "the export of a clone cannot share {}, which overlaps the template store {}, \
                 whose leased payloads nothing may change",
                key.display(),
                store.display()
            )
        )
    );
    assert_untouched(&acquired);
    // A directory beside the store is no part of it.
    let export = fixture.data_export("data");
    let request = with_export(request, &export);
    drop(fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap());
}

#[test]
fn exports_that_let_the_guest_turn_the_template_store_s_path_are_refused() {
    // The store's path leads through two links: `share/link`, to `hop/next`, which links to
    // `private`, which holds the store. Neither directory of a link holds any part of the store,
    // but a guest that could write in either could replace its link, and turn the path, by which
    // the host reaches the store, and OpenVMM the snapshots in it, elsewhere.
    let fixture = Fixture::in_store(
        |directory| {
            for name in ["share", "hop", "private"] {
                fs::create_dir(directory.join(name)).unwrap();
            }
            fs::create_dir(directory.join("share").join("work")).unwrap();
            let next = directory.join("hop").join("next");
            link_dir(&directory.join("private"), &next);
            let link = directory.join("share").join("link");
            link_dir(&next, &link);
            link.join("templates")
        },
        |_, _, _| {},
    );
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let request = tmpfs_request(&image, &binding);
    let resolved = |name: &str| filesystem::canonicalize(&fixture.directory.path().join(name));
    let (share, hop) = (resolved("share").unwrap(), resolved("hop").unwrap());
    // The store's path resolves outside both directories, so no export of either overlaps it.
    let store = filesystem::canonicalize(fixture.store.dir()).unwrap();
    assert_eq!(store, resolved("private").unwrap().join("templates"));
    // A writable export of either directory.
    for dir in [&share, &hop] {
        let export = export_plan(&[(dir, "/data", false)], &[], &[]);
        let request = with_export(request.clone(), &export);
        let plan = fixture.plan(&mut acquired, &request, NO_CONTENT);
        assert_eq!(
            refused(plan, Evidence::InvalidMappings),
            (
                ErrorKind::Unsupported,
                format!(
                    "the export of a clone cannot let its guest write in {}, through which the \
                     path of the template store {} leads, since the guest could then turn that \
                     path elsewhere, or keep the host from following it",
                    dir.display(),
                    fixture.store.dir().display()
                )
            )
        );
        assert_untouched(&acquired);
    }
    // Every directory of an aggregate is checked: a writable one beside a read-only directory
    // through which the store's path does not lead.
    let beside = fixture.exported("beside", &[]);
    let export = export_plan(
        &[(&beside, "/beside", true), (&hop, "/hop", false)],
        &[],
        &[],
    );
    assert_eq!(export.children.len(), 2);
    let plan = fixture.plan(
        &mut acquired,
        &with_export(request.clone(), &export),
        NO_CONTENT,
    );
    assert_eq!(
        refused(plan, Evidence::InvalidMappings),
        (
            ErrorKind::Unsupported,
            format!(
                "the export of a clone cannot let its guest write in {}, through which the path \
                 of the template store {} leads, since the guest could then turn that path \
                 elsewhere, or keep the host from following it",
                hop.display(),
                fixture.store.dir().display()
            )
        )
    );
    assert_untouched(&acquired);
    // A read-only export of the directory of a link, one in which the guest writes only a
    // directory beside the link, and one of a file beside the link, whose content alone the
    // guest writes, leave the link as it is.
    let work = share.join("work");
    let notes = share.join("notes.txt");
    fs::write(&notes, b"notes").unwrap();
    for mappings in [
        &[(share.as_path(), "/data", true)][..],
        &[
            (share.as_path(), "/data", true),
            (work.as_path(), "/work", false),
        ],
        &[(notes.as_path(), "/notes.txt", false)],
    ] {
        let export = export_plan(mappings, &[], &[]);
        let request = with_export(request.clone(), &export);
        drop(fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap());
    }
}

#[test]
fn exports_that_let_the_guest_block_the_template_store_s_path_are_refused() {
    // The store's path leads through `traverse` and its `..`, which Unix looks up in `traverse`:
    // a guest that could write there could make it unsearchable, and keep the host from the
    // store. Windows resolves `..` from the path's text, and never looks in `traverse`.
    let fixture = Fixture::in_store(
        |directory| {
            for name in ["traverse", "private"] {
                fs::create_dir(directory.join(name)).unwrap();
            }
            fs::create_dir(directory.join("traverse").join("work")).unwrap();
            directory
                .join("traverse")
                .join("..")
                .join("private")
                .join("templates")
        },
        |_, _, _| {},
    );
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let request = tmpfs_request(&image, &binding);
    let traverse = filesystem::canonicalize(&fixture.directory.path().join("traverse")).unwrap();
    // A writable export of the directory.
    let export = export_plan(&[(&traverse, "/data", false)], &[], &[]);
    let writable = with_export(request.clone(), &export);
    let plan = fixture.plan(&mut acquired, &writable, NO_CONTENT);
    if cfg!(windows) {
        drop(plan.unwrap());
    } else {
        assert_eq!(
            refused(plan, Evidence::InvalidMappings),
            (
                ErrorKind::Unsupported,
                format!(
                    "the export of a clone cannot let its guest write in {}, through which the \
                     path of the template store {} leads, since the guest could then turn that \
                     path elsewhere, or keep the host from following it",
                    traverse.display(),
                    fixture.store.dir().display()
                )
            )
        );
        assert_untouched(&acquired);
    }
    // A read-only export of the directory, and one in which the guest writes only a directory
    // in it, leave the directory as it is.
    let work = traverse.join("work");
    for mappings in [
        &[(traverse.as_path(), "/data", true)][..],
        &[
            (traverse.as_path(), "/data", true),
            (work.as_path(), "/work", false),
        ],
    ] {
        let export = export_plan(mappings, &[], &[]);
        let request = with_export(request.clone(), &export);
        drop(fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap());
    }
}

#[test]
fn exports_that_hold_the_sandbox_s_directory_are_refused() {
    // The host writes the clone's records in the sandbox's directory, by their paths, so a guest
    // that could reach the directory could change them, or replace it with a link, and turn the
    // host's writes elsewhere, into a leased snapshot, say.
    let fixture = Fixture::new();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let request = tmpfs_request(&image, &binding);
    let sandbox = acquired.lease.sandbox().to_owned();
    // The sandbox's directory, writable, and the directory that holds it, read-only.
    for (dir, read_only) in [
        (sandbox.clone(), false),
        (sandbox.parent().unwrap().to_owned(), true),
    ] {
        let export = export_plan(&[(&dir, "/data", read_only)], &[], &[]);
        let request = with_export(request.clone(), &export);
        let plan = fixture.plan(&mut acquired, &request, NO_CONTENT);
        assert_eq!(
            refused(plan, Evidence::InvalidMappings),
            (
                ErrorKind::Unsupported,
                format!(
                    "the export of a clone cannot share {}, which holds the directory of the \
                     clone's sandbox {}, in which the host writes the clone's records",
                    dir.display(),
                    sandbox.display()
                )
            )
        );
        assert_untouched(&acquired);
    }
    // A directory inside the sandbox's, among the caller's files, holds none of the host's
    // records.
    let shared = sandbox.join("shared");
    fs::create_dir(&shared).unwrap();
    let export = export_plan(&[(&shared, "/data", false)], &[], &[]);
    let request = with_export(request, &export);
    drop(fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap());
}

#[test]
fn files_in_the_sandbox_s_directory_are_refused() {
    // A file in the sandbox's directory itself may be one of the host's records, such as the
    // clone's binding, which names its lease, and which a guest could otherwise read or change.
    let fixture = Fixture::new();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let request = tmpfs_request(&image, &binding);
    let sandbox = acquired.lease.sandbox().to_owned();
    let record = sandbox.join(CLONE_BINDING_NAME);
    assert!(record.is_file(), "{}", record.display());
    // The record, read-only and writable, beside a directory that is no part of the sandbox's: an
    // aggregate's every file is checked.
    let beside = fixture.exported("beside", &[]);
    for read_only in [true, false] {
        let export = export_plan(
            &[
                (&beside, "/beside", true),
                (&record, "/clone.json", read_only),
            ],
            &[],
            &[],
        );
        assert_eq!(export.children.len(), 2);
        let plan = fixture.plan(
            &mut acquired,
            &with_export(request.clone(), &export),
            NO_CONTENT,
        );
        assert_eq!(
            refused(plan, Evidence::InvalidMappings),
            (
                ErrorKind::Unsupported,
                format!(
                    "the export of a clone cannot share {}, a file in the directory of the \
                     clone's sandbox {}, in which the host writes the clone's records",
                    record.display(),
                    sandbox.display()
                )
            )
        );
        assert_untouched(&acquired);
    }
    // A file in a directory inside the sandbox's, among the caller's files, is none of the host's
    // records.
    let shared = sandbox.join("shared");
    fs::create_dir(&shared).unwrap();
    let notes = shared.join("notes.txt");
    fs::write(&notes, b"notes").unwrap();
    let export = export_plan(&[(&notes, "/notes.txt", false)], &[], &[]);
    assert!(export.children[0].file);
    let request = with_export(request, &export);
    drop(fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap());
}

#[test]
fn the_export_of_a_clone_is_attached_and_mapped_as_a_cold_launch_attaches_and_maps_it() {
    let fixture = Fixture::new();
    let (mut export, [data, notes, settings, tools]) = fixture.aggregate_export("aggregate");
    let (work, secret) = (data.join("work"), data.join("secret"));
    let shown = secret.join("shown");
    let child = |index: usize, path: &Path, mode: &str| {
        format!("{index},{},{mode}", path.to_str().unwrap())
    };
    let text = |path: &Path| path.to_str().unwrap().to_owned();
    let attached = CloneExport::new(&export).unwrap();
    assert_eq!(attached.plan(), &export);
    // The restore attaches the aggregate with a cold launch's arguments: each directory and file
    // a child, by its index, with its mode, `rw` if a mapping writes it, a file marked as one,
    // then the paths that OpenVMM hides, shows again, and lets the guest write. The guest reads
    // `data` but for `work`, and `secret` hides all of it but `shown`.
    assert_eq!(attached.arguments(), export.openvmm_arguments());
    assert_eq!(
        attached.arguments(),
        [
            "--mount-aggregate".to_owned(),
            filesystem::GUEST_EXPORT.to_owned(),
            "--mount-child".to_owned(),
            child(0, &data, "rw"),
            "--mount-child".to_owned(),
            child(1, &notes, "rw,file"),
            "--mount-child".to_owned(),
            child(2, &settings, "ro,file"),
            "--mount-child".to_owned(),
            child(3, &tools, "ro"),
            "--mount-deny".to_owned(),
            text(&secret),
            "--mount-allow".to_owned(),
            text(&shown),
            "--mount-write".to_owned(),
            text(&work),
        ]
    );
    // Its table is a cold launch's, each mapping in the plan's order, by its child's index.
    let mapping = |source: &str, target: &str, read_only| HostMap {
        source: source.to_owned(),
        target: target.to_owned(),
        read_only,
    };
    assert_eq!(attached.table(), export.guest_table());
    assert_eq!(
        attached.table(),
        [
            mapping("0", "/data", true),
            mapping("1", "/notes.txt", false),
            mapping("2", "/settings.json", true),
            mapping("3", "/tools", true),
            mapping("0/work", "/work", false),
        ]
    );
    // An export whose operations run as the guest's caller says so last.
    export.owner = filesystem::Owner::Caller;
    let arguments = CloneExport::new(&export).unwrap().arguments();
    assert_eq!(
        arguments[arguments.len() - 2..],
        ["--mount-owner", "caller"]
    );
    // An aggregate that the guest only reads is attached read-only.
    let read_only = export_plan(&[(&data, "/data", true)], &[], &[]);
    assert_eq!(
        CloneExport::new(&read_only).unwrap().arguments(),
        [
            "--mount-aggregate".to_owned(),
            filesystem::GUEST_EXPORT.to_owned(),
            "--mount-child".to_owned(),
            child(0, &data, "ro"),
        ]
    );
}
