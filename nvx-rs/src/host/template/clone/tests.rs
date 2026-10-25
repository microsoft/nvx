//! Tests of the plans of clones: the checks of their images, uppers, and memory targets against
//! the generation that they lease, which read no content, the claims of their scratch images, and
//! the sparse scratch device of an upper in RAM.

use std::io::Read;
use std::time::Duration;

use super::*;
use crate::ErrorCode;
use crate::host::filesystem;
use crate::host::network::{Access as NetworkAccess, NetworkPolicy};
use crate::host::sandbox::distro::{DISTRO_POLICY_REVISION, PolicyVerdict};
use crate::host::sandbox::scratch::CLAIM_RELEASE_WAIT;
use crate::host::sandbox::{SandboxDistro, SandboxNetwork, SandboxSettings, SandboxUpper};
use crate::host::seal::{FileSeal, open_metadata_only};
use crate::host::store::{CLONE_SCRATCH_NAME, CallerFiles, LOG_NAME, SandboxStore};
use crate::host::template::test_support::{MIB, ManifestSpec, class, network_class};
use crate::host::template::{CaptureClass, HostTime};
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
/// The template's RAM, in bytes, as [`ManifestSpec::template`] records it.
const MEMORY: u64 = 512 * MIB;
/// The template's RAM capacity, in bytes, as [`ManifestSpec::template`] records it.
const MEMORY_CAPACITY: u64 = 2048 * MIB;
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
    create: &create_sparse,
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

/// Returns `path` as messages show it.
fn shown(path: &Path) -> String {
    PurePath::native(path.to_str().unwrap()).as_str().to_owned()
}

/// Returns the distro layer at `path`.
fn layer(path: &Path) -> SandboxLayer {
    SandboxLayer::new(LayerRole::Distro, path.to_str().unwrap(), UUID)
}

/// Returns the request of a clone of the image at `image`, which `binding` binds, over an upper in
/// RAM, with the template's RAM as its memory target.
fn tmpfs_request(image: &Path, binding: &DistroBinding) -> CloneRequest {
    CloneRequest {
        adapter: SandboxPlan::adapter(
            SandboxDistro::new(image.to_str().unwrap(), bound_image(binding).unwrap()),
            SandboxUpper::RAM,
        ),
        binding: binding.clone(),
        policy_revision: DISTRO_POLICY_REVISION,
        scratch: None,
        memory: MEMORY,
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

/// Returns the kind and the message of the error of `result`.
fn failure<T: fmt::Debug>(result: Result<T>) -> (ErrorKind, String) {
    let error = result.unwrap_err();
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
        let directory = tempfile::tempdir().unwrap();
        let physical = physical_block_size(directory.path());
        let store = Store::create(directory.path().join("templates")).unwrap();
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

    /// Stages the scratch image `name` of `length` bytes, and returns its path and its binding.
    fn scratch(&self, name: &str, length: u64) -> (PathBuf, ScratchBinding) {
        let path = self.path("scratches", name);
        stage(&path, length);
        let binding = ScratchBinding::bind(&path).unwrap();
        (path, binding)
    }

    /// Provisions the sandbox `name`, and leases the generation to it, a second after its
    /// capture.
    fn acquire(&self, name: &str) -> Acquired {
        let dir = self.path("sandboxes", name);
        let locked = SandboxStore::lock_for_provision(&dir, &files(), None).unwrap();
        fs::write(dir.join(CONFIG), b"{}").unwrap();
        drop(locked);
        self.store
            .acquire(
                self.generation.generation(),
                filesystem::canonicalize(&dir).unwrap(),
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
    ) -> Result<ClonePlan> {
        ClonePlan::new_with(
            &self.store,
            acquired,
            request,
            Instant::now() + CLAIM_RELEASE_WAIT,
            access,
        )
    }
}

/// Returns the path of the scratch device's file of a clone in RAM in the sandbox of `acquired`.
fn sparse_path(acquired: &Acquired) -> PathBuf {
    acquired.locked.dir().join(CLONE_SCRATCH_NAME)
}

/// Asserts that the sandbox of `acquired` holds no scratch device's file of a clone in RAM, and
/// that its lease still reserves its launch, as after a plan that failed.
fn assert_untouched(acquired: &Acquired) {
    assert!(!sparse_path(acquired).exists());
    assert_eq!(acquired.locked.reserved(), Some(acquired.lease.launch()));
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

#[test]
fn a_clone_in_ram_restores_its_lease_with_its_image_and_a_sparse_scratch_device() {
    let fixture = Fixture::new();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
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
    // The scratch device's file is a sparse file of the template's scratch length, private to the
    // current user, in the sandbox's directory, which the plan closed.
    let sparse = sparse_path(&acquired);
    assert!(matches!(plan.upper(), CloneUpper::Tmpfs { path } if *path == sparse));
    assert_eq!(plan.scratch_path(), sparse.as_path());
    let metadata = fs::metadata(&sparse).unwrap();
    assert_eq!(metadata.len(), SCRATCH_LEN);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;

        assert_eq!(metadata.mode() & 0o777, 0o600);
        assert!(
            metadata.blocks() * 512 < SCRATCH_LEN,
            "{}",
            metadata.blocks()
        );
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        /// `FILE_ATTRIBUTE_SPARSE_FILE`.
        const SPARSE: u32 = 0x200;
        assert_ne!(metadata.file_attributes() & SPARSE, 0);
    }
    let opened = File::open(&sparse).unwrap();
    assert_eq!(platform::foreign_writer_problem(&opened).unwrap(), None);
    drop(opened);
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
    assert_eq!(again.scratch_path(), sparse.as_path());
}

#[test]
fn the_scratch_device_of_a_clone_in_ram_is_fresh_and_goes_with_its_lease() {
    let fixture = Fixture::new();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let request = tmpfs_request(&image, &binding);
    // An earlier clone of the sandbox left its file, which the plan replaces.
    let sparse = sparse_path(&acquired);
    fs::write(&sparse, b"left by an earlier clone").unwrap();
    let plan = fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap();
    assert_eq!(plan.scratch_path(), sparse.as_path());
    let mut content = vec![1; 64];
    File::open(&sparse)
        .unwrap()
        .read_exact(&mut content)
        .unwrap();
    assert_eq!(content, [0; 64]);
    drop(plan);
    // The release of the lease, once the clone's VM has exited, removes it with the binding.
    assert_eq!(
        fixture.store.release(&mut acquired.locked).unwrap(),
        Some(acquired.lease.clone())
    );
    assert!(!sparse.exists());
    assert!(!acquired.locked.binding_path().exists());
    // The removal of the sandbox removes one that remains, as one of the store's files.
    fs::write(&sparse, b"").unwrap();
    let Acquired { locked, .. } = acquired;
    assert!(locked.remove().unwrap().is_none());
    assert!(!sparse.exists());
}

#[test]
fn a_scratch_device_that_the_plan_cannot_initialize_goes() {
    let fixture = Fixture::new();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let request = tmpfs_request(&image, &binding);
    // A creation that makes the file, and then fails to make it sparse, as one on a volume that
    // has no sparse files does.
    let create = |path: &Path| -> io::Result<File> {
        drop(create_new(path)?);
        Err(io::Error::other("the volume has no sparse files"))
    };
    let access = Access {
        create: &create,
        ..NO_CONTENT
    };
    let error = fixture.plan(&mut acquired, &request, access).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert_eq!(
        error.message(),
        format!(
            "cannot create the scratch device of the clone {}",
            shown(&sparse_path(&acquired))
        )
    );
    assert_eq!(
        std::error::Error::source(&error).unwrap().to_string(),
        "the volume has no sparse files"
    );
    assert_untouched(&acquired);
    // The next plan creates it afresh.
    let plan = fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap();
    assert_eq!(
        fs::metadata(plan.scratch_path()).unwrap().len(),
        SCRATCH_LEN
    );
}

#[test]
fn files_of_the_sandboxs_store_are_refused_as_images() {
    let fixture = Fixture::new();
    let mut acquired = fixture.acquire("sandbox");
    let sparse = sparse_path(&acquired);
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
    // An image that is the file of the scratch device of a clone in RAM, which a plan would
    // replace, is refused before anything replaces it...
    stage(&sparse, IMAGE_LEN);
    let binding = DistroBinding::import(&layer(&sparse), PolicyVerdict::ACCEPTED).unwrap();
    let staged = seal_of(&sparse);
    assert_eq!(
        failure(fixture.plan(&mut acquired, &tmpfs_request(&sparse, &binding), NO_CONTENT)),
        refusal("image", &sparse, &sparse)
    );
    assert_eq!(seal_of(&sparse), staged);
    // ...by whatever path the request names it: through a hard link...
    let hard = fixture.path("images", "hard.erofs");
    fs::hard_link(&sparse, &hard).unwrap();
    let hard_binding = DistroBinding::import(&layer(&hard), PolicyVerdict::ACCEPTED).unwrap();
    assert_eq!(
        failure(fixture.plan(
            &mut acquired,
            &tmpfs_request(&hard, &hard_binding),
            NO_CONTENT
        )),
        refusal("image", &hard, &sparse)
    );
    // ...or through a link among its directories.
    let alias = fixture.path("images", "alias");
    if symlink_dir(acquired.locked.dir(), &alias) {
        let aliased = alias.join(CLONE_SCRATCH_NAME);
        let aliased_binding =
            DistroBinding::import(&layer(&aliased), PolicyVerdict::ACCEPTED).unwrap();
        assert_eq!(
            failure(fixture.plan(
                &mut acquired,
                &tmpfs_request(&aliased, &aliased_binding),
                NO_CONTENT
            )),
            refusal("image", &aliased, &sparse)
        );
    }
    // A release of the lease removes that file, so a clone on a scratch image takes it neither
    // as its image, which is bound again, since the hard link changed its change time on
    // Linux...
    let binding = DistroBinding::import(&layer(&sparse), PolicyVerdict::ACCEPTED).unwrap();
    let staged = seal_of(&sparse);
    let (image, image_binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let (scratch, scratch_binding) = fixture.scratch("scratch.ext4", SCRATCH_LEN);
    let request = scratch_request(&sparse, &binding, &scratch, &scratch_binding);
    assert_eq!(
        failure(fixture.plan(&mut acquired, &request, NO_CONTENT)),
        refusal("image", &sparse, &sparse)
    );
    assert_eq!(seal_of(&sparse), staged);
    // ...nor as its scratch image, whose claim the refused plan releases.
    fs::remove_file(&sparse).unwrap();
    stage(&sparse, SCRATCH_LEN);
    let sparse_binding = ScratchBinding::bind(&sparse).unwrap();
    let request = scratch_request(&image, &image_binding, &sparse, &sparse_binding);
    assert_eq!(
        failure(fixture.plan(&mut acquired, &request, NO_CONTENT)),
        refusal("scratch image", &sparse, &sparse)
    );
    assert_eq!(fs::metadata(&sparse).unwrap().len(), SCRATCH_LEN);
    assert!(released(|| sparse_binding
        .check(&sparse, Instant::now())
        .is_ok()));
    fs::remove_file(&sparse).unwrap();
    // The store's other files are refused alike, such as its log, which a launch replaces, and
    // the temporary files of its records.
    for name in [LOG_NAME, ".runtime.json.1.tmp"] {
        let owned = acquired.locked.dir().join(name);
        stage(&owned, IMAGE_LEN);
        let binding = DistroBinding::import(&layer(&owned), PolicyVerdict::ACCEPTED).unwrap();
        assert_eq!(
            failure(fixture.plan(&mut acquired, &tmpfs_request(&owned, &binding), NO_CONTENT)),
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
    let sparse = sparse_path(&acquired);
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
    // A directory, which a plan could not replace, nor a release or a launch remove.
    for name in [CLONE_SCRATCH_NAME, LOG_NAME] {
        let owned = acquired.locked.dir().join(name);
        fs::create_dir(&owned).unwrap();
        assert_eq!(
            failure(fixture.plan(&mut acquired, &request, NO_CONTENT)),
            refusal(&owned)
        );
        assert!(owned.is_dir());
        fs::remove_dir(&owned).unwrap();
    }
    // A link to the directory of the images, through which the path of an image can pass, and
    // which a plan would replace, leaving that path naming nothing.
    if symlink_dir(image.parent().unwrap(), &sparse) {
        let through = sparse.join("distro.erofs");
        let through_binding =
            DistroBinding::import(&layer(&through), PolicyVerdict::ACCEPTED).unwrap();
        for request in [tmpfs_request(&through, &through_binding), request.clone()] {
            assert_eq!(
                failure(fixture.plan(&mut acquired, &request, NO_CONTENT)),
                refusal(&sparse)
            );
        }
        assert!(
            fs::symlink_metadata(&sparse)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(fs::metadata(&through).unwrap().is_file());
        #[cfg(unix)]
        fs::remove_file(&sparse).unwrap();
        #[cfg(windows)]
        fs::remove_dir(&sparse).unwrap();
    }
    assert_untouched(&acquired);
    // A control socket that a launch left is no image, and the launch removes it.
    #[cfg(unix)]
    let _socket = std::os::unix::net::UnixListener::bind(
        acquired.locked.dir().join(crate::host::store::SOCKET_NAME),
    )
    .unwrap();
    // The plan creates the file of the scratch device once nothing else is there.
    let plan = fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap();
    assert!(fs::symlink_metadata(plan.scratch_path()).unwrap().is_file());
}

#[test]
fn names_that_the_file_system_takes_for_those_of_the_stores_files_are_the_stores() {
    let fixture = Fixture::new();
    let (image, image_binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let dir = acquired.locked.dir().to_owned();
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
    for name in [CLONE_SCRATCH_NAME, LOG_NAME] {
        let cased = dir.join(name.to_ascii_uppercase());
        stage(&cased, IMAGE_LEN);
        let binding = DistroBinding::import(&layer(&cased), PolicyVerdict::ACCEPTED).unwrap();
        let owned = dir.join(name);
        let same = same_file(&cased, &owned);
        let result = fixture.plan(&mut acquired, &tmpfs_request(&cased, &binding), NO_CONTENT);
        if same {
            assert_eq!(failure(result), refusal("image", &cased, &owned));
        } else {
            drop(result.unwrap());
        }
        assert_eq!(fs::metadata(&cased).unwrap().len(), IMAGE_LEN);
        fs::remove_file(&cased).unwrap();
    }
    // So is a scratch image, which a release would remove as the store's file where the file
    // system takes it for that file, and keeps where it does not.
    let sparse = sparse_path(&acquired);
    let _ = fs::remove_file(&sparse);
    let cased = dir.join(CLONE_SCRATCH_NAME.to_ascii_uppercase());
    stage(&cased, SCRATCH_LEN);
    let binding = ScratchBinding::bind(&cased).unwrap();
    let request = scratch_request(&image, &image_binding, &cased, &binding);
    let same = same_file(&cased, &sparse);
    let result = fixture.plan(&mut acquired, &request, NO_CONTENT);
    if same {
        assert_eq!(failure(result), refusal("scratch image", &cased, &sparse));
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
        assert_eq!(
            failure(fixture.plan(&mut acquired, &tmpfs_request(&image, &binding), NO_CONTENT)),
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
    assert_eq!(plan.scratch_path(), checked.path());
    assert_eq!(
        checked.geometry().unwrap(),
        geometry(SCRATCH_LEN, fixture.physical)
    );
    // What the checks hold reads nothing, and the plan creates no scratch device of its own.
    let mut byte = [0];
    assert!(checked.file().read(&mut byte).is_err());
    assert!(plan.image().file().read(&mut byte).is_err());
    assert!(!sparse_path(&acquired).exists());
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
        failure(fixture.plan(&mut acquired, &request, NO_CONTENT)),
        changed(&scratch, "identity")
    );
    // So has a directory in its place. The image stays, under another name, so that the
    // directory cannot take its inode number, which is its file ID on Linux.
    let (scratch, scratch_binding) = fixture.scratch("directory.ext4", SCRATCH_LEN);
    fs::rename(&scratch, fixture.path("scratches", "moved.ext4")).unwrap();
    fs::create_dir(&scratch).unwrap();
    let request = scratch_request(&image, &binding, &scratch, &scratch_binding);
    assert_eq!(
        failure(fixture.plan(&mut acquired, &request, NO_CONTENT)),
        changed(&scratch, "identity")
    );
    // A binding of another kind names another file.
    let (scratch, scratch_binding) = fixture.scratch("kind.ext4", SCRATCH_LEN);
    let mut value = serde_json::to_value(&scratch_binding).unwrap();
    value["kind"] = "directory".into();
    let scratch_binding: ScratchBinding = serde_json::from_value(value).unwrap();
    let request = scratch_request(&image, &binding, &scratch, &scratch_binding);
    assert_eq!(
        failure(fixture.plan(&mut acquired, &request, NO_CONTENT)),
        changed(&scratch, "kind")
    );
    assert_untouched(&acquired);
}

#[test]
fn images_of_another_length_or_geometry_are_refused() {
    let fixture = Fixture::new();
    let mut acquired = fixture.acquire("sandbox");
    let mut refusal = |length: u64| {
        let (image, binding) = fixture.image("other.erofs", length);
        let failed =
            failure(fixture.plan(&mut acquired, &tmpfs_request(&image, &binding), NO_CONTENT));
        fs::remove_file(&image).unwrap();
        (shown(&image), failed)
    };
    // A clone's image has the placeholder's length.
    for length in [IMAGE_LEN - 512, IMAGE_LEN + 512, 2 * IMAGE_LEN] {
        let (image, failed) = refusal(length);
        assert_eq!(
            failed,
            (
                ErrorKind::Unsupported,
                format!(
                    "the image {image} is {length} bytes long, not the {IMAGE_LEN} bytes of the \
                     template's placeholder"
                )
            )
        );
    }
    // OpenVMM attaches no block device of another length.
    for length in [0, IMAGE_LEN + 1] {
        let (image, failed) = refusal(length);
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
    // the filesystem of this one.
    let fixture = Fixture::with(|class, spec, physical| {
        let scratch = class.scratch;
        let distro = geometry(IMAGE_LEN, other_physical(physical));
        set_geometries(class, spec, distro, scratch);
    });
    let mut acquired = fixture.acquire("sandbox");
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let physical = fixture.physical;
    assert_eq!(
        failure(fixture.plan(&mut acquired, &tmpfs_request(&image, &binding), NO_CONTENT)),
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
            failure(fixture.plan(&mut acquired, &request, NO_CONTENT)),
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
        failure(fixture.plan(&mut acquired, &request, NO_CONTENT)),
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
fn sandboxes_whose_filesystem_presents_another_scratch_geometry_are_refused() {
    // A template whose scratch presented physical blocks of another size than the filesystem of
    // the sandboxes serves no clone in RAM there, nor on a scratch image of that filesystem.
    let fixture = Fixture::with(|class, spec, physical| {
        let distro = class.distro;
        let scratch = geometry(SCRATCH_LEN, other_physical(physical));
        set_geometries(class, spec, distro, scratch);
    });
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let (physical, other) = (fixture.physical, other_physical(fixture.physical));
    assert_eq!(
        failure(fixture.plan(&mut acquired, &tmpfs_request(&image, &binding), NO_CONTENT)),
        (
            ErrorKind::Unsupported,
            format!(
                "the scratch device of the clone {}, in the sandbox's filesystem, presents \
                 logical blocks of 512 bytes and physical blocks of {physical} bytes, but the \
                 template's scratch presented 512 and {other} bytes",
                shown(&sparse_path(&acquired))
            )
        )
    );
    // The refused plan removed the file that it created.
    assert_untouched(&acquired);
    let (scratch, scratch_binding) = fixture.scratch("scratch.ext4", SCRATCH_LEN);
    let request = scratch_request(&image, &binding, &scratch, &scratch_binding);
    assert_eq!(
        failure(fixture.plan(&mut acquired, &request, NO_CONTENT)),
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
/// unlike the template's: an image, a scratch image, and the scratch device of a clone in RAM,
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
    let Some(huge_image) = huge_file(IMAGE_LEN) else {
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
    // The image: its binding and the check of the binding reach a file on hugetlbfs at its path.
    let image = fixture.path("images", "huge.erofs");
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
    assert_eq!(
        failure(fixture.plan(&mut acquired, &tmpfs_request(&image, &binding), access)),
        presented(format!("the image {}", shown(&image)), "placeholder")
    );
    // The scratch image: its binding and its check reach a file on hugetlbfs at its path.
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let huge_scratch = huge_file(SCRATCH_LEN).unwrap();
    let scratch = fixture.path("scratches", "huge.ext4");
    let open = |_: &Path| huge_scratch.try_clone();
    let scratch_access = scratch::Access { open: &open };
    let scratch_binding =
        bind_with(&PurePath::native(scratch.to_str().unwrap()), scratch_access).unwrap();
    let access = Access {
        scratch: scratch_access,
        ..NO_CONTENT
    };
    let request = scratch_request(&image, &binding, &scratch, &scratch_binding);
    assert_eq!(
        failure(fixture.plan(&mut acquired, &request, access)),
        presented(format!("the scratch image {}", shown(&scratch)), "scratch")
    );
    // The scratch device of a clone in RAM, which a sandbox on hugetlbfs creates there.
    let huge_device = huge_file(SCRATCH_LEN).unwrap();
    let create = |_: &Path| huge_device.try_clone();
    let access = Access {
        create: &create,
        ..NO_CONTENT
    };
    assert_eq!(
        failure(fixture.plan(&mut acquired, &tmpfs_request(&image, &binding), access)),
        presented(
            format!(
                "the scratch device of the clone {}, in the sandbox's filesystem,",
                shown(&sparse_path(&acquired))
            ),
            "scratch"
        )
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
            failure(fixture.plan(&mut acquired, &request, NO_CONTENT)),
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
fn requests_that_no_clone_takes_are_refused_before_anything_is_opened() {
    let fixture = Fixture::new();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let (scratch, scratch_binding) = fixture.scratch("scratch.ext4", SCRATCH_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let mut refusal = |request: &CloneRequest| {
        let (kind, message) = failure(fixture.plan(&mut acquired, request, NO_CONTENT));
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
    let message = refusal(&profile);
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
        refusal(&mistaken),
        format!(
            "the image {} is not the erofs layer with identity {UUID} that its distro binding \
             names",
            shown(&image)
        )
    );
    // Paths that OpenVMM would not open as the plan checks them.
    let at = |path: &str| CloneRequest {
        adapter: SandboxPlan::adapter(SandboxDistro::new(path, distro.image()), SandboxUpper::RAM),
        ..tmpfs.clone()
    };
    let message = refusal(&at("distro.erofs"));
    assert!(
        message.starts_with("the image of a clone must be absolute"),
        "{message}"
    );
    let message = refusal(&at(&format!("{},x", image.to_str().unwrap())));
    assert!(
        message.starts_with("disk paths containing commas or semicolons"),
        "{message}"
    );
    let message = refusal(&at(image.with_extension("vhdx").to_str().unwrap()));
    assert!(
        message.starts_with("disk paths ending in .vhdx"),
        "{message}"
    );
    let on_scratch = |path: &str| CloneRequest {
        adapter: SandboxPlan::adapter(distro.clone(), SandboxUpper::scratch(path)),
        scratch: Some(scratch_binding.clone()),
        ..tmpfs.clone()
    };
    let message = refusal(&on_scratch("scratch.ext4"));
    assert!(
        message.starts_with("the scratch image of a clone must be absolute"),
        "{message}"
    );
    let message = refusal(&on_scratch(scratch.with_extension("iso").to_str().unwrap()));
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
        refusal(&unbound),
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
        refusal(&bound),
        "the upper of a clone in RAM takes no scratch binding"
    );
    // None of them opened anything: the scratch image is free, and the lease still reserves the
    // clone's launch, which a plan of a request that a clone takes then serves.
    drop(scratch_binding.check(&scratch, Instant::now()).unwrap());
    assert_untouched(&acquired);
    drop(fixture.plan(&mut acquired, &tmpfs, NO_CONTENT).unwrap());
}

#[test]
fn networks_that_the_template_does_not_have_are_refused() {
    let fixture = Fixture::new();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let request = tmpfs_request(&image, &binding);
    let generation = fixture.generation.generation();
    let attached = with_network(request.clone(), Some(network(NetworkAccess::Allow)));
    assert_eq!(
        failure(fixture.plan(&mut acquired, &attached, NO_CONTENT)),
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

    // A template with a NIC serves the clones that attach its network.
    let fixture = Fixture::with(|class, spec, physical| {
        *class = network_class();
        *spec = ManifestSpec::with_network();
        let (distro, scratch) = (
            geometry(IMAGE_LEN, physical),
            geometry(SCRATCH_LEN, physical),
        );
        set_geometries(class, spec, distro, scratch);
    });
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let request = tmpfs_request(&image, &binding);
    let generation = fixture.generation.generation();
    assert_eq!(
        failure(fixture.plan(&mut acquired, &request, NO_CONTENT)),
        (
            ErrorKind::Unsupported,
            format!(
                "the template's generation {generation} has a NIC, which its clones restore with \
                 the network of the adapter's plan, but the plan has none"
            )
        )
    );
    assert_untouched(&acquired);
    let attached = with_network(request, Some(network(NetworkAccess::Allow)));
    drop(fixture.plan(&mut acquired, &attached, NO_CONTENT).unwrap());
}

#[test]
fn clones_are_planned_under_the_lock_that_acquired_their_lease_before_their_launch() {
    let fixture = Fixture::new();
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let request = tmpfs_request(&image, &binding);
    // A lease of another store.
    let other = Fixture::new();
    let mut elsewhere = other.acquire("sandbox");
    let (kind, message) = failure(fixture.plan(&mut elsewhere, &request, NO_CONTENT));
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
    let (kind, message) = failure(fixture.plan(&mut relocked, &request, NO_CONTENT));
    assert_eq!(kind, ErrorKind::Other, "{message}");
    assert!(
        message.starts_with("a clone is planned under the lock that acquired its lease"),
        "{message}"
    );
    assert!(!sparse_path(&relocked).exists());
    // The lock that acquired a lease, once the launch that the lease reserves has begun.
    let mut acquired = fixture.acquire("begun");
    let key = fixture.store.read_key(&fixture.generation).unwrap();
    drop(
        acquired
            .locked
            .begin_launch(acquired.lease.launch(), key.runtime(), &[7; CAPABILITY_LEN])
            .unwrap(),
    );
    let (kind, message) = failure(fixture.plan(&mut acquired, &request, NO_CONTENT));
    assert_eq!(kind, ErrorKind::Other, "{message}");
    assert!(!sparse_path(&acquired).exists());
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
        assert_eq!(
            failure(fixture.plan(&mut acquired, &tmpfs_request(&link, &binding), NO_CONTENT)),
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
            failure(fixture.plan(&mut acquired, &request, NO_CONTENT)),
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
    assert_eq!(
        failure(fixture.plan(&mut acquired, &tmpfs_request(&path, &binding), NO_CONTENT)),
        (
            ErrorKind::Unavailable,
            format!("the image {} {problem}", shown(&path))
        )
    );
    assert_untouched(&acquired);
}
