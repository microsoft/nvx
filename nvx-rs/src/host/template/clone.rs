//! The plan of a clone, [`ClonePlan`]: the generation of a template that a clone leased, with the
//! clone's own image, upper, and memory target, which the plan checks against the generation
//! before the clone's launch, so that a request that the template cannot serve fails before any
//! VMM starts.

#[cfg(all(test, any(target_os = "linux", windows)))]
mod tests;

use std::fmt;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::key::TemplateKey;
use super::manifest::BlockGeometry;
use super::plan::MEMORY_BLOCK_SIZE;
use super::store::{Acquired, Generation, Lease, Store};
use crate::contract;
use crate::error::{Error, ErrorKind, Result};
use crate::host::filesystem;
use crate::host::instance::generate_capability;
use crate::host::paths;
use crate::host::platform;
use crate::host::sandbox::distro::{self, CheckedDistro, DistroBinding, DistroFormat};
use crate::host::sandbox::plan::bound_image;
use crate::host::sandbox::scratch::{self, CheckedScratch, ScratchBinding};
use crate::host::sandbox::{
    LayerRole, SandboxLayer, SandboxPlan, block_geometry, reject_container_path, reject_disk_path,
};
use crate::host::seal::FileSeal;
use crate::host::store::LockedStore;
use crate::host::store::files::{create_new, remove_if_present};
use crate::host::store::regular::{open_regular, was_removed};
use crate::protocol::CAPABILITY_LEN;

/// Bytes in a mebibyte, the unit in which messages give the size of memory blocks.
const MIB: u64 = 1024 * 1024;

/// How messages name the file of the `scratch` block device of a clone whose upper is in RAM.
const SPARSE_WHAT: &str = "scratch device of the clone";

/// What the clone of a sandbox restores of the generation of a template that it leased: the
/// adapter's plan of the clone's image and upper, the binding of the image, the binding of the
/// scratch image of an upper on one, and the memory target, which [`ClonePlan::new`] checks
/// against the generation.
#[derive(Debug, Clone)]
pub struct CloneRequest {
    /// The adapter's plan, which [`SandboxPlan::adapter`] makes, of the clone's image, its
    /// `distro`, over its upper: in RAM, a tmpfs, as
    /// [`SandboxUpper::RAM`](crate::host::sandbox::SandboxUpper::RAM) plans it, or on the
    /// caller's scratch image. Its network is the one of the template's NIC, which every restore
    /// of the template attaches, or none for a template without a NIC.
    pub adapter: SandboxPlan,
    /// The binding of the image.
    pub binding: DistroBinding,
    /// The revision of the distro policy that the caller accepts, such as
    /// [`DISTRO_POLICY_REVISION`](crate::host::sandbox::distro::DISTRO_POLICY_REVISION), which
    /// [`DistroBinding::check`] requires of the binding's verdict.
    pub policy_revision: u32,
    /// The binding of the scratch image of an upper on one, or `None` for an upper in RAM.
    pub scratch: Option<ScratchBinding>,
    /// The memory target, in bytes: the RAM of the clone's VM once restored, to which the restore
    /// expands the template's RAM in whole blocks.
    pub memory: u64,
}

/// The upper of a clone's plan, with the file of the clone's `scratch` block device, which every
/// restore of a template attaches in place of the template's scratch.
#[derive(Debug)]
pub enum CloneUpper {
    /// An upper in RAM, a tmpfs, which leaves the `scratch` block device unused: its file is a
    /// sparse file of the template's scratch length,
    /// [`CLONE_SCRATCH_NAME`](crate::host::store::CLONE_SCRATCH_NAME) in the sandbox's directory,
    /// which the plan created, and which [`Store::release`] removes once the clone's VM has
    /// exited, as a [removal](crate::host::store#removal) of the sandbox does.
    Tmpfs {
        /// The sparse file.
        path: PathBuf,
    },
    /// An upper on the caller's scratch image, which the plan checked and claimed: the claim
    /// lasts as long as the checked image, as [`ScratchBinding::check`] describes.
    Scratch(CheckedScratch),
}

/// The plan of a clone: the generation of a template that the clone leased, with the clone's own
/// image, upper, and memory target, checked against the generation, and a fresh capability, which
/// [`new`](Self::new) makes under the lock of the sandbox that acquired the lease, before the
/// clone's launch.
///
/// A restore of a template attaches the template's two block devices, its `distro` and its
/// `scratch`, with files of the clone's, which OpenVMM opens itself, by their paths, as
/// [Block devices by path](Store#block-devices-by-path) describes, and whose geometries it
/// compares with those that the template recorded. The plan checks each of the clone's inputs as
/// OpenVMM will find it, and holds what the checks opened:
///
/// - The image, the clone's `distro`, must pass the check of its binding,
///   [`DistroBinding::check`], and present the geometry of the template's placeholder, as its
///   key records it: the placeholder's length, and the same logical and physical block sizes,
///   which [`CheckedDistro::geometry`] reads through the checked file. At its path, it must be a
///   regular file, not a link, nor on Windows any other reparse point, that is the file that the
///   check found, that belongs to the current user or to an administrator, and that no one else
///   may change. The plan holds the file that the check opened, for reading alone.
/// - Neither the image nor a scratch image may be a file that the sandbox's store owns, by any
///   path, through a link among its directories, say, or a hard link: the store's transitions
///   replace or remove its files, as a plan replaces the file of the scratch device of a clone in
///   RAM, [`CLONE_SCRATCH_NAME`](crate::host::store::CLONE_SCRATCH_NAME), a launch its log, and a
///   release the clone binding. The store creates nothing but regular files under its names, and
///   its control socket, so the plan also refuses a link or a directory under one of them,
///   through which the path of an image could pass.
/// - An upper in RAM, a tmpfs, leaves the `scratch` block device unused, which the restore
///   attaches all the same: the plan creates its file,
///   [`CLONE_SCRATCH_NAME`](crate::host::store::CLONE_SCRATCH_NAME) in the sandbox's directory, a
///   sparse file of the template's scratch length, private to the current user, in place of one
///   that an earlier clone of the sandbox left, and closes it. The filesystem of the sandbox's
///   directory must present the template's scratch geometry.
/// - An upper on the caller's scratch image must pass the check of its binding,
///   [`ScratchBinding::check`], which checks the image's identity, kind, and length and claims it
///   for the clone, and present the geometry of the template's scratch, its length included,
///   which [`CheckedScratch::geometry`] reads through the checked file. At its path, it must be a
///   regular file, not a link, nor on Windows any other reparse point, that is the file that the
///   check found, and that no one else may change. The plan holds the checked image, and with it
///   the claim. Nothing reads, formats, or copies the image.
/// - The memory target must be whole blocks of [`MEMORY_BLOCK_SIZE`], and of the template's own
///   memory blocks, from the template's RAM to its RAM capacity.
/// - The capability is fresh: the plan draws it for the clone's launch.
///
/// The plan's checks read the metadata of the clone's files, never their content.
///
/// ```no_run
/// use std::time::{Duration, Instant};
///
/// use nvx::host::sandbox::distro::{DISTRO_POLICY_REVISION, DistroBinding};
/// use nvx::host::sandbox::{SandboxDistro, SandboxPlan, SandboxUpper};
/// use nvx::host::store::CallerFiles;
/// use nvx::host::template::{
///     CloneRequest, ClonePlan, Generation, HostTime, MEMORY_BLOCK_SIZE, Store,
/// };
///
/// # fn clone(
/// #     store: &Store,
/// #     generation: &Generation,
/// #     image: SandboxDistro,
/// #     binding: DistroBinding,
/// # ) -> nvx::Result<()> {
/// let files = CallerFiles::new("config.json")?;
/// let mut acquired = store.acquire(
///     generation.generation(),
///     "/var/lib/example/sandboxes/clone",
///     &files,
///     &HostTime::now()?,
///     None,
/// )?;
/// let request = CloneRequest {
///     adapter: SandboxPlan::adapter(image, SandboxUpper::RAM),
///     binding,
///     policy_revision: DISTRO_POLICY_REVISION,
///     scratch: None,
///     memory: 8 * MEMORY_BLOCK_SIZE,
/// };
/// let deadline = Instant::now() + Duration::from_secs(5);
/// let plan = ClonePlan::new(store, &mut acquired, &request, deadline)?;
/// // ... the clone's launch, under `acquired.locked`, as the launch that the lease reserves, of
/// // `plan.snapshot()` with `plan.image().path()` and `plan.scratch_path()` ...
/// # let _ = plan;
/// # Ok(())
/// # }
/// ```
pub struct ClonePlan {
    /// The lease of the generation.
    lease: Lease,
    /// The leased generation, as the store's index lists it.
    generation: Generation,
    /// The generation's compatibility key.
    key: TemplateKey,
    /// The generation's snapshot, which the clone restores.
    snapshot: PathBuf,
    /// The clone's image, which the check of its binding opened.
    image: CheckedDistro,
    /// The clone's upper, with the file of its `scratch` block device.
    upper: CloneUpper,
    /// The memory target, in bytes.
    memory: u64,
    /// The capability of the clone's launch.
    capability: [u8; CAPABILITY_LEN],
}

/// How a plan reaches the clone's files: as the checks of their bindings reach them, and as it
/// creates the file of the `scratch` block device of an upper in RAM; or, in tests, through opens
/// that read no content, or that reach files on filesystems of other block sizes.
#[derive(Clone, Copy)]
struct Access<'a> {
    /// How the check of the image's binding reaches the image.
    distro: distro::Access<'a>,
    /// How the check of the scratch image's binding reaches the image.
    scratch: scratch::Access<'a>,
    /// Creates the file of the `scratch` block device of an upper in RAM, open for writing.
    create: &'a dyn Fn(&Path) -> io::Result<File>,
}

impl Access<'static> {
    /// The access of every plan outside tests.
    const CHECKED: Self = Self {
        distro: distro::Access::GUARDED,
        scratch: scratch::Access::WRITABLE,
        create: &create_sparse,
    };
}

impl ClonePlan {
    /// Plans the clone of the sandbox of `acquired`, which leased a generation of `store`, as
    /// `request` asks, waiting until `deadline` for a claim of its scratch image that another
    /// launch holds to be released.
    ///
    /// The plan is made under the lock that acquired the lease, which `acquired` holds, before the
    /// launch that the lease reserves, [`Lease::launch`], which the caller then runs under the
    /// same lock: the sandbox records no VM and no launch, so no VM of an earlier clone holds a
    /// file that the plan replaces. It takes `acquired` exclusively while it plans, since it may
    /// create a file in the sandbox's directory, as the transitions of a sandbox's store take its
    /// lock, and borrows nothing of it once it returns. It reads the generation's key,
    /// [`Store::read_key`], without computing its identity again, and checks the generation's
    /// snapshot, [`Store::snapshot_dir`], then checks the clone's files, as the [type](Self)
    /// describes.
    ///
    /// Fails, and holds nothing, keeping no file that it created, checking in this order:
    ///
    /// - with [`ErrorKind::Unsupported`] for a request that no clone takes: a plan of the sandbox
    ///   profile, with workload settings and its shares; a plan whose image is not the format and
    ///   identity that the binding names, as for a binding whose identity is not a UUID in its
    ///   canonical text; an image or a scratch image whose path is relative, which OpenVMM would
    ///   resolve against the directory in which it runs, holds a comma or a semicolon, which
    ///   OpenVMM's block device options would split, or whose name ends in `.vhd`, `.vhdx`,
    ///   `.vmgs`, or `.iso`, which OpenVMM would not attach as a raw image; and an upper on a
    ///   scratch image without the image's binding, or one in RAM with a binding;
    /// - with [`ErrorKind::Other`] for a lease of another store, and a lock that is not the one
    ///   that acquired the lease, before the launch that it reserves began;
    /// - with [`ErrorKind::NotFound`] if the store no longer indexes the leased generation, with
    ///   [`ErrorKind::Io`] if it indexes it with another key than the lease names, and as
    ///   [`Store::index`], [`Store::read_key`], and [`Store::snapshot_dir`] fail;
    /// - with [`ErrorKind::Unsupported`] for a request that the template cannot serve: a template
    ///   whose key names other contracts than this build speaks, the boot and control contracts
    ///   of the runtime that captured it included; a plan with a network for a template without a
    ///   NIC, or without one for a template with a NIC; and a memory target that is not whole
    ///   blocks of [`MEMORY_BLOCK_SIZE`] and of the template's memory blocks, from the template's
    ///   RAM to its RAM capacity;
    /// - with [`ErrorKind::Unavailable`] for a link or a directory under a name that the sandbox's
    ///   store owns, and with [`ErrorKind::Io`] if the store's files cannot be listed or examined;
    /// - as [`DistroBinding::check`] and [`CheckedDistro::geometry`] fail, with
    ///   [`ErrorKind::Unsupported`] for an image of another geometry than the template's
    ///   placeholder, its length included, with [`ErrorKind::Unavailable`] for an image that is
    ///   not, at its path, the regular file that the check found, of the current user or an
    ///   administrator, and that no one else may change, and with [`ErrorKind::Unsupported`] for
    ///   an image that is a file of the sandbox's store;
    /// - for an upper in RAM, with [`ErrorKind::Unsupported`] for a sandbox whose path holds a
    ///   comma or a semicolon, or whose filesystem presents another geometry than the template's
    ///   scratch, and with [`ErrorKind::Io`] if the sparse file cannot be created, sized, or
    ///   checked;
    /// - for an upper on a scratch image, as [`ScratchBinding::check`] and
    ///   [`CheckedScratch::geometry`] fail, with [`ErrorKind::Unsupported`] for an image of
    ///   another geometry than the template's scratch, its length included, with
    ///   [`ErrorKind::Unavailable`] for an image that is not, at its path, the regular file that
    ///   the check found, of the current user or an administrator, and that no one else may
    ///   change, and with [`ErrorKind::Unsupported`] for an image that is a file of the sandbox's
    ///   store.
    pub fn new(
        store: &Store,
        acquired: &mut Acquired,
        request: &CloneRequest,
        deadline: Instant,
    ) -> Result<Self> {
        Self::new_with(store, acquired, request, deadline, Access::CHECKED)
    }

    /// Plans a clone as [`new`](Self::new) does, reaching its files through `access`.
    fn new_with(
        store: &Store,
        acquired: &mut Acquired,
        request: &CloneRequest,
        deadline: Instant,
        access: Access<'_>,
    ) -> Result<Self> {
        check_request(request)?;
        let Acquired { lease, locked } = acquired;
        check_lease(store, lease, locked)?;
        let generation = store
            .index()?
            .get(lease.generation())
            .cloned()
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::NotFound,
                    format!(
                        "the template store {} holds no generation {}, which the lease {} names",
                        store.dir().display(),
                        lease.generation(),
                        lease.id()
                    ),
                )
            })?;
        if generation.key() != lease.key() {
            return Err(Error::io(format!(
                "the generation {} of the template store {} has the key {}, not the key {} that \
                 the lease {} names",
                generation.generation(),
                store.dir().display(),
                generation.key(),
                lease.key(),
                lease.id()
            )));
        }
        let key = store.read_key(&generation)?;
        let snapshot = store.snapshot_dir(&generation)?;
        check_template(request, &key, &generation)?;
        let owned = StoreFiles::of(locked)?;
        let image = check_image(request, &key, &owned, access)?;
        let capability = generate_capability()?;
        let upper = match (request.adapter.upper().scratch_path(), &request.scratch) {
            (Some(path), Some(binding)) => CloneUpper::Scratch(check_scratch(
                path, binding, &key, &owned, deadline, access,
            )?),
            _ => CloneUpper::Tmpfs {
                path: create_scratch(locked, &key, access)?,
            },
        };
        Ok(Self {
            lease: lease.clone(),
            generation,
            key,
            snapshot,
            image,
            upper,
            memory: request.memory,
            capability,
        })
    }

    /// Returns the lease of the generation, which reserves the clone's launch,
    /// [`Lease::launch`].
    pub fn lease(&self) -> &Lease {
        &self.lease
    }

    /// Returns the leased generation, as the store's index lists it.
    pub fn generation(&self) -> &Generation {
        &self.generation
    }

    /// Returns the generation's compatibility key, as the store read it from the generation's
    /// payload.
    pub fn key(&self) -> &TemplateKey {
        &self.key
    }

    /// Returns the generation's snapshot, which the clone restores.
    pub fn snapshot(&self) -> &Path {
        &self.snapshot
    }

    /// Returns the clone's image, which the check of its binding opened, for reading alone, and
    /// which OpenVMM opens again at its path, [`CheckedDistro::path`], as the clone's `distro`.
    pub fn image(&self) -> &CheckedDistro {
        &self.image
    }

    /// Returns the clone's upper, with the file of its `scratch` block device.
    pub fn upper(&self) -> &CloneUpper {
        &self.upper
    }

    /// Returns the file of the clone's `scratch` block device, which OpenVMM opens at this path:
    /// the sparse file [`CLONE_SCRATCH_NAME`](crate::host::store::CLONE_SCRATCH_NAME) in the
    /// sandbox's directory for an upper in RAM, and the caller's scratch image otherwise.
    pub fn scratch_path(&self) -> &Path {
        match &self.upper {
            CloneUpper::Tmpfs { path } => path,
            CloneUpper::Scratch(checked) => checked.path(),
        }
    }

    /// Returns the memory target, in bytes.
    pub fn memory(&self) -> u64 {
        self.memory
    }

    /// Returns the capability of the clone's launch, which the plan drew, and which the VMM
    /// receives, as [`LockedStore::begin_launch`] stores it.
    pub fn capability(&self) -> &[u8; CAPABILITY_LEN] {
        &self.capability
    }
}

impl fmt::Debug for ClonePlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The capability authenticates control sessions, so it is never shown.
        formatter
            .debug_struct("ClonePlan")
            .field("lease", &self.lease)
            .field("generation", &self.generation)
            .field("snapshot", &self.snapshot)
            .field("image", &self.image)
            .field("upper", &self.upper)
            .field("memory", &self.memory)
            .finish_non_exhaustive()
    }
}

/// Refuses a request that no clone takes, as [`ClonePlan::new`] describes, without reading or
/// opening anything.
fn check_request(request: &CloneRequest) -> Result<()> {
    let adapter = &request.adapter;
    if adapter.settings().is_some() {
        return Err(Error::unsupported(
            "a clone restores an adapter's plan, not a plan of the sandbox profile, whose \
             workload settings no template holds",
        ));
    }
    let distro = adapter.distro();
    let binding = &request.binding;
    if bound_image(binding)? != distro.image() {
        return Err(Error::unsupported(format!(
            "the image {} is not the {} layer with identity {} that its distro binding names",
            distro.path().display(),
            binding.format(),
            binding.identity()
        )));
    }
    check_disk_path(distro.path(), "image")?;
    match (adapter.upper().scratch_path(), &request.scratch) {
        (Some(path), Some(_)) => check_disk_path(path, "scratch image"),
        (None, None) => Ok(()),
        (Some(path), None) => Err(Error::unsupported(format!(
            "the upper of a clone on the scratch image {} needs the image's scratch binding",
            path.display()
        ))),
        (None, Some(_)) => Err(Error::unsupported(
            "the upper of a clone in RAM takes no scratch binding",
        )),
    }
}

/// Refuses the path of the file of a clone's block device, which messages call `what`, that
/// OpenVMM would not open as the plan checks it: a relative one, which it would resolve against the
/// directory in which it runs, one with a comma or a semicolon, which its block device options
/// would split, and one whose name ends in `.vhd`, `.vhdx`, `.vmgs`, or `.iso`, which it would not
/// attach as a raw image.
fn check_disk_path(path: &Path, what: &str) -> Result<()> {
    let shown = path.display().to_string();
    if !path.is_absolute() {
        return Err(Error::unsupported(format!(
            "the {what} of a clone must be absolute, since OpenVMM would resolve it against the \
             directory in which it runs: {shown}"
        )));
    }
    reject_disk_path(path, &shown)?;
    reject_container_path(path, &shown)
}

/// Refuses a lease of a store other than `store`, and a lock, `locked`, that is not the one that
/// acquired `lease`, before the launch that it reserves began.
fn check_lease(store: &Store, lease: &Lease, locked: &LockedStore) -> Result<()> {
    if lease.store() != store.id() {
        return Err(Error::other(format!(
            "the lease {} is one of the template store {}, not of this one, {}",
            lease.id(),
            lease.store(),
            store.id()
        )));
    }
    if locked.dir() != lease.sandbox() || locked.reserved() != Some(lease.launch()) {
        return Err(Error::other(format!(
            "a clone is planned under the lock that acquired its lease, before the launch that \
             the lease reserves begins, but the lock of the sandbox in {} reserves no launch {} \
             of the lease {}",
            locked.dir().display(),
            lease.launch(),
            lease.id()
        )));
    }
    Ok(())
}

/// Refuses a request that the template of `generation`, whose key is `key`, cannot serve, as
/// [`ClonePlan::new`] describes, without reading or opening anything.
fn check_template(
    request: &CloneRequest,
    key: &TemplateKey,
    generation: &Generation,
) -> Result<()> {
    let name = generation.generation();
    // The key's contracts are those through which its clones are restored; the runtime's are
    // those through which this build boots and drives the guest that the template holds.
    let runtime = key.runtime();
    if !key.speaks_current_contracts()
        || runtime.boot_contract != contract::BOOT_CONTRACT_VERSION
        || runtime.control_contract != contract::CONTROL_CONTRACT_REVISION
        || runtime.control_session != contract::CONTROL_SESSION_PROTOCOL_VERSION
    {
        return Err(Error::unsupported(format!(
            "the template's generation {name} speaks other contracts than this build, which \
             restores none of its clones"
        )));
    }
    // A network that OpenVMM attaches no device for is no NIC, as a capture class's.
    let attached = match request.adapter.network() {
        None => false,
        Some(network) => !network.openvmm_arguments()?.is_empty(),
    };
    if attached != key.has_network() {
        return Err(Error::unsupported(if attached {
            format!(
                "the adapter's plan of a clone attaches a network, but the template's generation \
                 {name} has no NIC"
            )
        } else {
            format!(
                "the template's generation {name} has a NIC, which its clones restore with the \
                 network of the adapter's plan, but the plan has none"
            )
        }));
    }
    let memory = request.memory;
    let block = key.memory_block_size();
    if memory < key.memory()
        || memory > key.memory_capacity()
        || !memory.is_multiple_of(MEMORY_BLOCK_SIZE)
        || !memory.is_multiple_of(block)
    {
        return Err(Error::unsupported(format!(
            "a clone of the template's generation {name} cannot restore with {memory} bytes of \
             RAM: its memory target must be whole blocks of {} MiB, and of the template's memory \
             blocks of {block} bytes, from the template's {} bytes of RAM to its capacity of {} \
             bytes",
            MEMORY_BLOCK_SIZE / MIB,
            key.memory(),
            key.memory_capacity()
        )));
    }
    Ok(())
}

/// Checks the image of `request` against its binding, through `access`, and then its geometry,
/// against the placeholder's that `key` records, the image at its path, and that it is none of
/// `owned`, the files of the sandbox's store, as [`ClonePlan`] describes, and returns the file
/// that the binding's check opened.
fn check_image(
    request: &CloneRequest,
    key: &TemplateKey,
    owned: &StoreFiles,
    access: Access<'_>,
) -> Result<CheckedDistro> {
    let path = request.adapter.distro().path();
    let shown = path.display();
    let refused = |problem: String| Error::unavailable(format!("the image {shown} {problem}"));
    let text = path
        .to_str()
        .ok_or_else(|| refused("has a path that is not valid Unicode".to_owned()))?;
    let binding = &request.binding;
    let layer = match binding.format() {
        DistroFormat::Erofs => SandboxLayer::new(LayerRole::Distro, text, binding.identity()),
        DistroFormat::Gpt => SandboxLayer::gpt(LayerRole::Distro, text, binding.identity()),
    };
    let checked = binding.check_with(&layer, request.policy_revision, access.distro)?;
    check_geometry(
        checked.geometry()?,
        key.distro(),
        &format!("the image {shown}"),
        "placeholder",
    )?;
    let identity = identity_of(checked.file())
        .map_err(|error| refused(format!("cannot be identified: {error}")))?;
    check_at_path(path, identity, refused)?;
    owned.check(identity, "image", path)?;
    Ok(checked)
}

/// Checks and claims the scratch image at `path` against `binding`, through `access`, waiting
/// until `deadline` for a claim that another launch holds to be released, and then checks its
/// geometry, against the template's scratch that `key` records, the image at its path, and that
/// it is none of `owned`, the files of the sandbox's store, as [`ClonePlan`] describes, and
/// returns the image that the binding's check opened and claimed.
fn check_scratch(
    path: &Path,
    binding: &ScratchBinding,
    key: &TemplateKey,
    owned: &StoreFiles,
    deadline: Instant,
    access: Access<'_>,
) -> Result<CheckedScratch> {
    let shown = path.display();
    let checked = binding.check_with(path, deadline, access.scratch)?;
    check_geometry(
        checked.geometry()?,
        key.scratch(),
        &format!("the scratch image {shown}"),
        "scratch",
    )?;
    let refused =
        |problem: String| Error::unavailable(format!("the scratch image {shown} {problem}"));
    let identity = identity_of(checked.file())
        .map_err(|error| refused(format!("cannot be identified: {error}")))?;
    check_at_path(path, identity, refused)?;
    owned.check(identity, "scratch image", path)?;
    Ok(checked)
}

/// The regular files that the store of a sandbox owns, with their identities, which no image or
/// scratch image of a clone may be.
struct StoreFiles(Vec<(PathBuf, (u64, u128))>);

impl StoreFiles {
    /// Returns the regular files that the store of the sandbox of `locked` owns now, as
    /// [`LockedStore::owned_entries`] lists its entries: its files, among them the file of the
    /// scratch device of a clone in RAM, which a plan replaces, its log, which a launch replaces,
    /// and the clone binding, which a release removes, and its temporary files.
    ///
    /// The store creates nothing but regular files under its names, and its control socket, so a
    /// link or a directory among its entries is refused: the path of an image could pass through
    /// it as through a directory, and a transition that replaces or removes it would leave that
    /// path naming nothing. Other kinds of files, such as a socket, are no image.
    fn of(locked: &LockedStore) -> Result<Self> {
        let failed = |error: io::Error| {
            Error::io(format!(
                "cannot list the files of the sandbox's store in {}",
                locked.dir().display()
            ))
            .with_source(error)
        };
        let mut files = Vec::new();
        for entry in locked.owned_entries().map_err(failed)? {
            let shown = paths::normalized(&entry);
            let absent = |error: &io::Error| {
                error.kind() == io::ErrorKind::NotFound || was_removed(&entry, error)
            };
            let failed = |error: io::Error| {
                Error::io(format!(
                    "cannot examine {}, a file of the sandbox's store",
                    shown.display()
                ))
                .with_source(error)
            };
            let metadata = match fs::symlink_metadata(&entry) {
                Ok(metadata) => metadata,
                Err(error) if absent(&error) => continue,
                Err(error) => return Err(failed(error)),
            };
            let kind = metadata.file_type();
            if kind.is_dir() || kind.is_symlink() || filesystem::is_reparse_point(&metadata) {
                return Err(Error::unavailable(format!(
                    "{} is a link or a directory, which the sandbox's store never creates there",
                    shown.display()
                )));
            }
            if !kind.is_file() {
                continue;
            }
            let file = match open_regular(&entry) {
                Ok(file) => file,
                Err(error) if absent(&error) => continue,
                Err(error) => return Err(failed(error)),
            };
            let identity = identity_of(&file).map_err(failed)?;
            files.push((shown, identity));
        }
        Ok(Self(files))
    }

    /// Refuses the clone's image or scratch image at `path`, which messages call `what`, and
    /// whose identity is `identity`, if it is one of these files, by whatever path `path` names
    /// it, as through a link among its directories, or a hard link.
    fn check(&self, identity: (u64, u128), what: &str, path: &Path) -> Result<()> {
        match self.0.iter().find(|(_, owned)| *owned == identity) {
            None => Ok(()),
            Some((shown, _)) => Err(Error::unsupported(format!(
                "the {what} {} is {}, a file of the sandbox's store, which its transitions \
                 replace or remove",
                path.display(),
                shown.display()
            ))),
        }
    }
}

/// Creates the file of the `scratch` block device of a clone whose upper is in RAM, in the
/// sandbox of `locked`, through `access`: a sparse file of the length of the template's scratch,
/// which `key` records, whose geometry must be the scratch's, and which is checked at its path as
/// the capture of a template checks its own, as [`ClonePlan`] describes. Returns its path, once
/// the file is closed. It removes the file if any step fails, its creation included, which can
/// fail once it made the file, as a file that cannot be made sparse does.
fn create_scratch(locked: &LockedStore, key: &TemplateKey, access: Access<'_>) -> Result<PathBuf> {
    let path = locked.clone_scratch_path();
    let shown = path.display();
    reject_disk_path(&path, &shown.to_string())?;
    let failed = |error: io::Error| {
        Error::io(format!("cannot create the {SPARSE_WHAT} {shown}")).with_source(error)
    };
    let created = (|| {
        let file = (access.create)(&path).map_err(&failed)?;
        let scratch = key.scratch();
        file.set_len(scratch.length).map_err(&failed)?;
        check_geometry(
            block_geometry(&file, &path, SPARSE_WHAT)?,
            scratch,
            &format!("the {SPARSE_WHAT} {shown}, in the sandbox's filesystem,"),
            "scratch",
        )?;
        let identity = identity_of(&file).map_err(&failed)?;
        drop(file);
        check_at_path(&path, identity, |problem| {
            Error::io(format!("the {SPARSE_WHAT} {shown} {problem}"))
        })
    })();
    match created {
        Ok(()) => Ok(path),
        Err(error) => {
            let _ = remove_if_present(&path);
            Err(error)
        }
    }
}

/// Creates the file of the `scratch` block device of a clone whose upper is in RAM at `path`, a
/// sparse file private to the current user, open for writing, in place of one that an earlier
/// clone of the sandbox left.
fn create_sparse(path: &Path) -> io::Result<File> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let file = create_new(path)?;
    platform::make_sparse(&file)?;
    Ok(file)
}

/// Refuses `found`, the geometry of the file of a clone's block device, which messages call
/// `what`, unless it is `expected`, the geometry that the template's `device` presented.
fn check_geometry(
    found: BlockGeometry,
    expected: BlockGeometry,
    what: &str,
    device: &str,
) -> Result<()> {
    if found.length != expected.length {
        return Err(Error::unsupported(format!(
            "{what} is {} bytes long, not the {} bytes of the template's {device}",
            found.length, expected.length
        )));
    }
    if found != expected {
        return Err(Error::unsupported(format!(
            "{what} presents logical blocks of {} bytes and physical blocks of {} bytes, but the \
             template's {device} presented {} and {} bytes",
            found.logical_block_size,
            found.physical_block_size,
            expected.logical_block_size,
            expected.physical_block_size
        )));
    }
    Ok(())
}

/// Returns the identity of the file that `file` holds open, as its [`FileSeal`] records it: its
/// device or volume, and its file ID, the 128-bit one of `FILE_ID_INFO` on Windows, which a file
/// system such as ReFS keeps unique where the 64-bit file index is not.
fn identity_of(file: &File) -> io::Result<(u64, u128)> {
    let seal = FileSeal::of(file)?;
    Ok((seal.device(), seal.file_id()))
}

/// Checks the file at `path` as OpenVMM will open it, by its path: a regular file, not a link,
/// nor on Windows any other reparse point, whose identity is `expected`, the identity of the file
/// that a check found, and that no one but the current user, or an administrator, may change.
/// Fails with the error that `refused` makes of the problem that it found.
fn check_at_path(
    path: &Path,
    expected: (u64, u128),
    refused: impl Fn(String) -> Error,
) -> Result<()> {
    let file = open_regular(path).map_err(|error| refused(format!("cannot be used: {error}")))?;
    let identity =
        identity_of(&file).map_err(|error| refused(format!("cannot be identified: {error}")))?;
    if identity != expected {
        return Err(refused(
            "changed while it was checked: its path names another file".to_owned(),
        ));
    }
    if let Some(problem) = platform::foreign_writer_problem(&file)
        .map_err(|error| refused(format!("cannot be checked: {error}")))?
    {
        return Err(refused(problem));
    }
    Ok(())
}
