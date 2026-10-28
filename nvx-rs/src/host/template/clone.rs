//! The plan of a clone, [`ClonePlan`]: the generation of a template that a clone leased, with the
//! clone's own image, upper, memory target, and export, which the plan checks against the
//! generation before the clone's launch, so that a request that the template cannot serve fails
//! before any VMM starts; and the clone's launch, [`ClonePlan::launch`], OpenVMM's restore of the
//! generation's snapshot with what the plan checked.

#[cfg(all(test, any(target_os = "linux", windows)))]
mod tests;

mod export;
mod failure;
mod launch;
mod reattach;
mod record;
mod records;
mod restore;

pub use self::failure::{
    CloneFailure, Evidence, FailureRecord, Outcome, Progress, Stage, Teardown, status_evidence,
};
pub use self::launch::Restoring;
pub use self::reattach::{ReattachedClone, reattach_clone};
pub use self::records::{CloneRecords, FAILURE_FORMAT};
pub use self::restore::{
    Activated, CloneControl, MAX_ACTIVATION_SESSIONS, READY_TO_START_TIMEOUT, RESTORE_READY_EVENT,
    RESTORE_TO_READY_TIMEOUT, RestoreDeadlines, Started,
};

use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use self::export::DirectoryExport;
use self::failure::{Attribute, Fault, host_evidence};
use super::files::{open_regular, was_removed};
use super::key::TemplateKey;
use super::manifest::BlockGeometry;
use super::plan::MEMORY_BLOCK_SIZE;
use super::store::{Acquired, Generation, Lease, Store};
use crate::activation::{ActivationRecord, Journal, OverlayMode, WorkloadView};
use crate::contract;
use crate::error::{Error, ErrorCode, ErrorKind, Result};
use crate::host::filesystem::{self, ExportPlan};
use crate::host::instance::generate_capability;
use crate::host::platform;
use crate::host::python::path::PurePath;
use crate::host::sandbox::distro::{
    self, CheckedDistro, DistroBinding, DistroFormat, PolicyResult,
};
use crate::host::sandbox::plan::bound_image;
use crate::host::sandbox::scratch::{self, CheckedScratch, ScratchBinding};
use crate::host::sandbox::{
    LayerRole, SandboxLayer, SandboxPlan, SandboxUpper, block_geometry, reject_container_path,
    reject_disk_path,
};
use crate::host::seal::FileSeal;
use crate::host::store::LockedStore;
use crate::host::store::files::{create_new, remove_if_present};
use crate::protocol::{CAPABILITY_LEN, HostMap};

/// Bytes in a mebibyte, the unit in which messages give the size of memory blocks.
const MIB: u64 = 1024 * 1024;

/// The most links that [`path_directories`] follows in a path, more than a Linux host, 40, or a
/// Windows host, 63, follows in one.
const MAX_PATH_LINKS: usize = 64;

/// How messages name the file of the `scratch` block device of a clone whose upper is in RAM.
const SPARSE_WHAT: &str = "scratch device of the clone";

/// What the clone of a sandbox restores of the generation of a template that it leased: the
/// adapter's plan of the clone's image and upper, the binding of the image, the binding of the
/// scratch image of an upper on one, the export of the host paths that the clone shares with its
/// root, if any, and the memory target, which [`ClonePlan::new`] checks against the generation,
/// and the settings of the clone's activation that the template does not fix, which the clone's
/// [activation record](ClonePlan::activation) names: the workload's view of its VM, the overlay's
/// mode, the journal policy of a scratch image, and the bound of a GPT disk's first partition.
#[derive(Debug, Clone)]
pub struct CloneRequest {
    /// The adapter's plan, which [`SandboxPlan::adapter`] makes, of the clone's image, its
    /// `distro`, an EROFS layer or a GPT disk, over its upper: in RAM, a tmpfs, as
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
    /// The export of the host paths that the clone shares with its root, which
    /// [`filesystem::plan`] planned for the container root,
    /// [`ReservedTargets::CONTAINER_ROOT`](crate::host::filesystem::ReservedTargets::CONTAINER_ROOT),
    /// or `None` for a clone that shares none, whose restore leaves the template's filesystem slot
    /// dormant. The slot binds one directory, so the plan must export exactly one, and no file,
    /// as [`ClonePlan`] describes.
    pub export: Option<ExportPlan>,
    /// The memory target, in bytes: the RAM of the clone's VM once restored, to which the restore
    /// expands the template's RAM in whole blocks.
    pub memory: u64,
    /// The view that the clone's workload has of its VM: its hostname, the machine ID that it
    /// sees, and the identity in which every execution runs, unless the executions name their
    /// own.
    pub workload: WorkloadView,
    /// The mode of the clone's overlay, which selects its options.
    pub overlay: OverlayMode,
    /// Whether the filesystem of the scratch image of an upper on one may have a journal. An
    /// upper in RAM has none, so the record of a clone in RAM leaves the policy out.
    pub journal: Journal,
    /// The most bytes that the first partition of a GPT disk may hold, at least 1, which the
    /// guest checks as it reads the disk's partition table. An EROFS layer has no partitions, so
    /// the record of a clone of one leaves the bound out.
    pub first_partition_bound: u64,
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
/// image, upper, and memory target, checked against the generation, the clone's activation
/// record, and a fresh capability, which [`new`](Self::new) makes under the lock of the sandbox
/// that acquired the lease, before the clone's launch.
///
/// A restore of a template attaches the template's two block devices, its `distro` and its
/// `scratch`, with files of the clone's, which OpenVMM opens itself, by their paths, as
/// [Block devices by path](Store#block-devices-by-path) describes, and whose geometries it
/// compares with those that the template recorded. The plan checks each of the clone's inputs as
/// OpenVMM will find it, and holds what the checks opened:
///
/// - The image, the clone's `distro`, an EROFS layer or a GPT disk, as its binding names it, must
///   pass the check of its binding, [`DistroBinding::check`], and present the geometry of the
///   template's placeholder, as its key records it: the placeholder's length, and the same logical
///   and physical block sizes, which [`CheckedDistro::geometry`] reads through the checked file,
///   whatever the placeholder's format. At its path, it must be a regular file, not a link, nor on
///   Windows any other reparse point, that is the file that the check found, that belongs to the
///   current user or to an administrator, and that no one else may change. The plan holds the
///   file that the check opened, for reading alone. As at a cold launch, the guest reads and
///   checks a GPT disk's partition table, with `nvx::gpt`, before it assembles the root, so the
///   plan reads none of it.
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
/// - The export, if the request has one, must be a plan that [`filesystem::plan`] could have made
///   for the container root, as [`ExportPlan::check`] finds it, of exactly one child, a
///   directory: a template's filesystem slot is dormant, and a restore binds one directory to it,
///   which is then the export's root, in which the sources of the plan's binds name their paths
///   without the child's index, as the mapping table, [`table`](Self::table), sends them. The
///   directory may neither lie inside the template store's directory nor hold it, by the path
///   that the plan resolved, so that no guest reaches the payload of a generation through it. The
///   launch checks the plan's host objects again, with [`ExportPlan::verify`], before it restores
///   the clone. A request without an export leaves the slot dormant.
/// - The activation record, which the clone's guest applies in place of the configuration that
///   the template captured, names the restore, with a fresh transaction ID, the clone's image,
///   upper, export, and settings, the template's network, and the resources that the clone must
///   have, as [`activation`](Self::activation) describes. Its settings must keep the limits of a
///   record, as [`Configuration::check`] checks them.
/// - The capability is fresh: the plan draws it for the clone's launch, [`launch`](Self::launch).
///
/// The plan's checks read the metadata of the clone's files, never their content.
///
/// ```no_run
/// use std::path::Path;
/// use std::time::{Duration, Instant};
///
/// use nvx::activation::{Journal, MachineIdMode, OverlayMode, WorkloadView};
/// use nvx::host::sandbox::distro::{DISTRO_POLICY_REVISION, DistroBinding};
/// use nvx::host::sandbox::{SandboxDistro, SandboxPlan, SandboxUpper};
/// use nvx::host::store::{CallerFiles, RuntimeIdentity};
/// use nvx::host::template::{
///     CloneRequest, ClonePlan, Generation, HostTime, MEMORY_BLOCK_SIZE, Store,
/// };
///
/// # fn clone(
/// #     store: &Store,
/// #     generation: &Generation,
/// #     image: SandboxDistro,
/// #     binding: DistroBinding,
/// #     runtime: &RuntimeIdentity,
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
///     export: None,
///     memory: 8 * MEMORY_BLOCK_SIZE,
///     workload: WorkloadView {
///         hostname: b"clone".to_vec(),
///         machine_id: MachineIdMode::Image,
///         identity: None,
///     },
///     overlay: OverlayMode::UntrustedLowers,
///     journal: Journal::Allowed,
///     first_partition_bound: 64 * 1024,
/// };
/// let deadline = Instant::now() + Duration::from_secs(5);
/// let plan = ClonePlan::new(store, &mut acquired, &request, deadline)?;
/// // OpenVMM restores the generation's snapshot under the runtime that captured the template.
/// let restoring = plan.launch(&mut acquired, Path::new("openvmm"), runtime)?;
/// // ... the restore, whose VMM sends its restore-ready event to `restoring.ready_endpoint()`,
/// // serves its control console at `restoring.starting().endpoint()`, and whose guest applies
/// // `restoring.activation()` ...
/// # let _ = restoring;
/// # Ok(())
/// # }
/// ```
///
/// [`Configuration::check`]: crate::activation::Configuration::check
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
    /// The clone's activation record.
    activation: ActivationRecord,
    /// The capability of the clone's launch.
    capability: [u8; CAPABILITY_LEN],
    /// The clone's devices, which its restore attaches: the adapter's plan of its image and
    /// network, over the file of its `scratch` block device.
    devices: SandboxPlan,
    /// The clone's export, which its restore binds to the template's dormant filesystem slot, if
    /// it has one.
    export: Option<DirectoryExport>,
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
    /// same lock, with [`launch`](Self::launch): the sandbox records no VM and no launch, so no VM of an earlier clone holds a
    /// file that the plan replaces. It takes `acquired` exclusively while it plans, since it may
    /// create a file in the sandbox's directory, as the transitions of a sandbox's store take its
    /// lock, and borrows nothing of it once it returns. It reads the generation's key,
    /// [`Store::read_key`], without computing its identity again, and checks the generation's
    /// snapshot, [`Store::snapshot_dir`], then checks the clone's files, as the [type](Self)
    /// describes.
    ///
    /// Fails with a [`CloneFailure`] at [`Stage::Plan`], and holds nothing, keeping no file that
    /// it created. The failure's record holds the evidence of the check that failed, as below, no
    /// transaction, and no VM, [`Teardown::NoVm`], and the plan records it nowhere: the caller
    /// holds it, and releases the lease. The evidence of a failure of the host's own operations is
    /// [`Evidence::ResourceExhaustion`] if an I/O error among the error's sources says that the
    /// host ran out of storage, memory, or descriptors, and [`Evidence::HostFailure`] otherwise.
    /// The plan checks in this order:
    ///
    /// - with [`ErrorKind::Unsupported`] for a request that no clone takes: a plan of the sandbox
    ///   profile, with workload settings and its shares, [`Evidence::InvalidSettings`]; a plan
    ///   whose image is not the format and identity that the binding names, as for a binding whose
    ///   identity is not a UUID in its canonical text, [`Evidence::InvalidFormat`]; an image or a
    ///   scratch image whose path is relative, which OpenVMM would resolve against the directory in
    ///   which it runs, holds a comma or a semicolon, which OpenVMM's block device options would
    ///   split, or whose name ends in `.vhd`, `.vhdx`, `.vmgs`, or `.iso`, which OpenVMM would not
    ///   attach as a raw image, and an upper on a scratch image without the image's binding, or one
    ///   in RAM with a binding, [`Evidence::InvalidSettings`]; and an export plan that
    ///   [`filesystem::plan`] could not have made for the container root, as [`ExportPlan::check`]
    ///   finds it, such as one that maps paths but exports no directory or file, or exports some
    ///   but maps no path, and one that exports more than one directory or file, or a file, which
    ///   the template's dormant filesystem slot does not bind, [`Evidence::InvalidMappings`];
    /// - with [`ErrorKind::Other`] for a lease of another store, and a lock that is not the one
    ///   that acquired the lease, before the launch that it reserves began,
    ///   [`Evidence::HostFailure`];
    /// - with [`ErrorKind::Unsupported`] for an export whose directory, by the path that its plan
    ///   resolved, lies inside the template store's directory, [`Store::dir`], by the path that
    ///   it resolves to, or holds it, or that lets the guest write in a directory through which
    ///   the store's path leads, as the host resolves it, links and `..` included, which the guest
    ///   could then turn elsewhere, or keep the host from following, [`Evidence::InvalidMappings`],
    ///   and with [`ErrorKind::Io`] if the store's directory, or a directory through which its
    ///   path leads, cannot be resolved, the host's evidence;
    /// - with [`ErrorKind::NotFound`] if the store no longer indexes the leased generation, and as
    ///   [`Store::index`] fails, the host's evidence; with [`ErrorKind::Io`] if it indexes it with
    ///   another key than the lease names, [`Evidence::MalformedGeneration`]; and as
    ///   [`Store::read_key`] and [`Store::snapshot_dir`] fail, once the store's directory of
    ///   generations passed their check, whose failure is the host's: the host's evidence for an
    ///   I/O error, and [`Evidence::MalformedGeneration`] for a generation whose payload, key, or
    ///   snapshot is missing, is not private to the current user, or is malformed;
    /// - with [`ErrorKind::Unsupported`] for a request that the template cannot serve: a template
    ///   whose key names other contracts than this build speaks, the boot and control contracts
    ///   of the runtime that captured it included, [`Evidence::IncompatibleHost`]; a plan with a
    ///   network for a template without a NIC, or without one for a template with a NIC,
    ///   [`Evidence::InvalidSettings`]; and a memory target that is not whole blocks of
    ///   [`MEMORY_BLOCK_SIZE`] and of the template's memory blocks, from the template's RAM to its
    ///   RAM capacity, [`Evidence::InvalidTargets`];
    /// - with [`ErrorKind::Io`] for a template whose command line names the DNS server of its
    ///   guest's resolver in another form than OpenVMM writes: more than once, without a NIC, or
    ///   other than as one IP address in its canonical spelling, [`Evidence::MalformedGeneration`];
    /// - with [`ErrorKind::Unsupported`] for settings that no activation record holds, as
    ///   [`Configuration::check`] finds them, such as a GPT disk's first-partition bound of 0, a
    ///   hostname that is empty, longer than
    ///   [`MAX_HOSTNAME_BYTES`](crate::activation::MAX_HOSTNAME_BYTES), or holds a NUL, and a
    ///   workload identity whose UID or GID is 0, [`Evidence::InvalidActivate`], and with
    ///   [`ErrorKind::Other`] if the transaction ID of the clone's restore cannot be drawn, the
    ///   host's evidence;
    /// - with [`ErrorKind::Unavailable`] for a link or a directory under a name that the sandbox's
    ///   store owns, and with [`ErrorKind::Io`] if the store's files cannot be listed or examined,
    ///   the host's evidence;
    /// - as [`DistroBinding::check`] and [`CheckedDistro::geometry`] fail: with
    ///   [`ErrorKind::Unavailable`] for a binding that holds a verdict of another revision of the
    ///   distro policy than the request accepts, [`Evidence::StaleBinding`], and with
    ///   [`ErrorKind::Unsupported`] for one whose verdict rejects the image,
    ///   [`Evidence::AuthorizationDenied`]; with [`ErrorKind::Unavailable`] for an image that
    ///   changed, was replaced, or went since its binding was made, or whose manifest did, and for
    ///   an image that is not, at its path, the regular file that the check found, of the current
    ///   user or an administrator, and that no one else may change,
    ///   [`Evidence::SuppliedIdentity`], and with [`ErrorKind::Io`] if the image cannot be opened
    ///   at its path otherwise, or identified or examined once open, the host's evidence; with
    ///   [`ErrorKind::Unsupported`] for an image of another length than the template's
    ///   placeholder, [`Evidence::SuppliedLength`], or of other block sizes, or of a geometry that
    ///   OpenVMM refuses, [`Evidence::SuppliedGeometry`]; and with
    ///   [`ErrorKind::Unsupported`] for an image that is a file of the sandbox's store,
    ///   [`Evidence::InvalidSettings`];
    /// - for an upper in RAM, with [`ErrorKind::Unsupported`] for a sandbox whose path holds a
    ///   comma or a semicolon, [`Evidence::HostFailure`], or whose filesystem presents another
    ///   geometry than the template's scratch, [`Evidence::IncompatibleHost`], and with
    ///   [`ErrorKind::Io`] if the sparse file cannot be created, sized, or checked, the host's
    ///   evidence;
    /// - for an upper on a scratch image, as [`ScratchBinding::check`] and
    ///   [`CheckedScratch::geometry`] fail: with [`ErrorKind::TimedOut`] and
    ///   [`ErrorCode::ScratchClaimed`] for an image that another launch still claims,
    ///   [`Evidence::UnusableScratch`]; with [`ErrorKind::Unavailable`] for an image that changed,
    ///   was replaced, or went since its binding was made, or that is not, at its path, the regular
    ///   file that the check found, of the current user or an administrator, and that no one else
    ///   may change, [`Evidence::SuppliedIdentity`], and with [`ErrorKind::Io`] if the image cannot
    ///   be opened at its path otherwise, or identified or examined once open, the host's
    ///   evidence; with [`ErrorKind::Unsupported`] for an image of another length than the
    ///   template's scratch, [`Evidence::SuppliedLength`], or of other block sizes,
    ///   [`Evidence::SuppliedGeometry`]; and with [`ErrorKind::Unsupported`] for an image that is a
    ///   file of the sandbox's store, or whose path is not Unicode, [`Evidence::InvalidSettings`].
    ///
    /// [`Configuration::check`]: crate::activation::Configuration::check
    pub fn new(
        store: &Store,
        acquired: &mut Acquired,
        request: &CloneRequest,
        deadline: Instant,
    ) -> Result<Self, CloneFailure> {
        Self::new_with(store, acquired, request, deadline, Access::CHECKED)
    }

    /// Plans a clone as [`new`](Self::new) does, reaching its files through `access`.
    fn new_with(
        store: &Store,
        acquired: &mut Acquired,
        request: &CloneRequest,
        deadline: Instant,
        access: Access<'_>,
    ) -> Result<Self, CloneFailure> {
        let lease = acquired.lease.clone();
        Self::plan(store, acquired, request, deadline, access).map_err(|fault| {
            let record = FailureRecord::new(
                &lease,
                None,
                (Stage::Plan, Progress::Planned),
                fault.evidence,
                None,
                Teardown::NoVm,
                &fault.error,
            );
            CloneFailure::new(fault.error, record)
        })
    }

    /// Plans a clone as [`new`](Self::new) does, reaching its files through `access`, and
    /// returns the fault of the check that failed.
    fn plan(
        store: &Store,
        acquired: &mut Acquired,
        request: &CloneRequest,
        deadline: Instant,
        access: Access<'_>,
    ) -> Result<Self, Fault> {
        let export = check_request(request)?;
        let Acquired { lease, locked } = acquired;
        check_lease(store, lease, locked).attribute(Evidence::HostFailure)?;
        if let Some(export) = &export {
            // The plan resolved the export's paths, so the store's path is resolved as well, so
            // that no other spelling of it hides an overlap, and so are the directories through
            // which it leads, in which the guest must not write.
            let dir = store.dir();
            let unresolved = |error: io::Error| {
                Error::io(format!(
                    "cannot resolve the template store {}",
                    dir.display()
                ))
                .with_source(error)
            };
            let resolved = filesystem::canonicalize(dir)
                .map_err(unresolved)
                .on_host()?;
            let through = path_directories(dir).map_err(unresolved).on_host()?;
            export
                .check_outside(dir, &resolved, &through)
                .attribute(Evidence::InvalidMappings)?;
        }
        let generation = store
            .index()
            .on_host()?
            .get(lease.generation())
            .cloned()
            .ok_or_else(|| {
                Fault::new(
                    Evidence::HostFailure,
                    Error::new(
                        ErrorKind::NotFound,
                        format!(
                            "the template store {} holds no generation {}, which the lease {} \
                             names",
                            store.dir().display(),
                            lease.generation(),
                            lease.id()
                        ),
                    ),
                )
            })?;
        if generation.key() != lease.key() {
            return Err(Fault::new(
                Evidence::MalformedGeneration,
                Error::io(format!(
                    "the generation {} of the template store {} has the key {}, not the key {} \
                     that the lease {} names",
                    generation.generation(),
                    store.dir().display(),
                    generation.key(),
                    lease.key(),
                    lease.id()
                )),
            ));
        }
        // A store whose directory of generations is not private fails every generation: the
        // host's, not the generation's.
        store.check_generations_dir().on_host()?;
        let key = store.read_key(&generation).map_err(generation_fault)?;
        let snapshot = store.snapshot_dir(&generation).map_err(generation_fault)?;
        check_template(request, &key, &generation)?;
        let activation = record::build(request, lease, &key)?;
        let owned = StoreFiles::of(locked).on_host()?;
        let image = check_image(request, &key, &owned, access)?;
        let capability = generate_capability().on_host()?;
        let upper = match (
            request.adapter.upper().pure_scratch_path(),
            &request.scratch,
        ) {
            (Some(path), Some(binding)) => CloneUpper::Scratch(check_scratch(
                path, binding, &key, &owned, deadline, access,
            )?),
            _ => CloneUpper::Tmpfs {
                path: create_scratch(locked, &key, access)?,
            },
        };
        let devices = devices_of(request, &upper);
        Ok(Self {
            lease: lease.clone(),
            generation,
            key,
            snapshot,
            image,
            upper,
            memory: request.memory,
            activation,
            capability,
            devices,
            export,
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

    /// Returns the export plan of the host paths that the clone shares with its root, which its
    /// restore binds to the template's dormant filesystem slot, or `None` for a clone that shares
    /// none, whose slot stays dormant.
    pub fn export(&self) -> Option<&ExportPlan> {
        self.export.as_ref().map(DirectoryExport::plan)
    }

    /// Returns the clone's mapping table, which its restore sequence sends once the clone is
    /// ready, before its sandbox starts, as [`Activated::start`] describes: the binds of the
    /// export plan, in their order, each from its path in the exported directory, without the
    /// child's index, which a directory export's root does not list, and with an empty source for
    /// the directory itself. A clone without an export has an empty table, and the sequence sends
    /// none.
    pub fn table(&self) -> Vec<HostMap> {
        self.export
            .as_ref()
            .map_or_else(Vec::new, DirectoryExport::table)
    }

    /// Returns the clone's activation record, which its guest applies in place of the
    /// configuration that the template captured, and whose identity,
    /// [`ActivationRecord::identity`], the guest's Ready reports.
    ///
    /// The record holds the restore section, with the restore's transaction ID, 16 random bytes
    /// that the plan drew, and the template that the [lease](Lease::template) names; the image, in
    /// the format that its binding names, with the identity of its binding: an EROFS layer, with
    /// the UUID that the guest expects of its filesystem, or a GPT disk, with the GUID that the
    /// guest expects of the disk, each if the adapter's plan names one, and the request's bound of
    /// the disk's first partition; the upper, a tmpfs, or the scratch image, with the identity of
    /// its binding and the request's journal policy; the request's overlay mode and workload view;
    /// the export, if the request has one, a directory, with the identity of its plan, and the
    /// number of its mappings, the binds of the plan, from 1 to
    /// [`MAX_MAPPINGS`](crate::activation::MAX_MAPPINGS), or no export and no mappings; the
    /// network of the template's NIC, if it has one, with the identity of the policy that OpenVMM
    /// enforces there, as the template's key records it, the DNS server with which the template's
    /// guest configured its resolver at boot, if the template's
    /// [command line](TemplateKey::command_line) names one, unpublished listeners, and the URL of
    /// the policy's proxy as the guest sees it, if any; and the template's processors, all
    /// online, and the memory target, as the resources that the clone must have.
    ///
    /// The identity of a binding, or of an export plan, is the SHA-256 digest of a domain of its
    /// kind and of its serde form, a few hundred bytes of metadata: the plan reads no content to
    /// compute it.
    pub fn activation(&self) -> &ActivationRecord {
        &self.activation
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
            .field("activation", &self.activation)
            .field("export", &self.export())
            .finish_non_exhaustive()
    }
}

/// Returns the devices of the clone of `request`, whose upper is `upper`: the adapter's plan of its
/// image and network, over the file of its `scratch` block device, which every restore attaches,
/// the sparse file of an upper in RAM included.
fn devices_of(request: &CloneRequest, upper: &CloneUpper) -> SandboxPlan {
    let adapter = &request.adapter;
    let scratch = match upper {
        // The sandbox's path, which its lease names, is valid Unicode.
        CloneUpper::Tmpfs { path } => {
            SandboxUpper::scratch_at(PurePath::native(&path.to_string_lossy()))
        }
        CloneUpper::Scratch(_) => adapter.upper().clone(),
    };
    SandboxPlan::adapter(adapter.distro().clone(), scratch).with_network(adapter.network().cloned())
}

/// Refuses a request that no clone takes, as [`ClonePlan::new`] describes, without reading or
/// opening anything, and returns the request's export, if it has one, which the checks admitted.
fn check_request(request: &CloneRequest) -> Result<Option<DirectoryExport>, Fault> {
    let adapter = &request.adapter;
    if adapter.settings().is_some() {
        return Err(Fault::new(
            Evidence::InvalidSettings,
            Error::unsupported(
                "a clone restores an adapter's plan, not a plan of the sandbox profile, whose \
                 workload settings no template holds",
            ),
        ));
    }
    let distro = adapter.distro();
    let binding = &request.binding;
    if bound_image(binding).attribute(Evidence::InvalidFormat)? != distro.image() {
        return Err(Fault::new(
            Evidence::InvalidFormat,
            Error::unsupported(format!(
                "the image {} is not the {} layer with identity {} that its distro binding names",
                distro.path().display(),
                binding.format(),
                binding.identity()
            )),
        ));
    }
    check_disk_path(distro.pure_path(), "image").attribute(Evidence::InvalidSettings)?;
    match (adapter.upper().pure_scratch_path(), &request.scratch) {
        (Some(path), Some(_)) => {
            check_disk_path(path, "scratch image").attribute(Evidence::InvalidSettings)?;
        }
        (None, None) => {}
        (Some(path), None) => {
            return Err(Fault::new(
                Evidence::InvalidSettings,
                Error::unsupported(format!(
                    "the upper of a clone on the scratch image {path} needs the image's scratch \
                     binding"
                )),
            ));
        }
        (None, Some(_)) => {
            return Err(Fault::new(
                Evidence::InvalidSettings,
                Error::unsupported("the upper of a clone in RAM takes no scratch binding"),
            ));
        }
    }
    request
        .export
        .as_ref()
        .map(DirectoryExport::new)
        .transpose()
        .attribute(Evidence::InvalidMappings)
}

/// Returns the fault of `error`, with which the template store's read of the key or of the
/// snapshot of the leased generation failed, once the store's directory of generations passed its
/// check: the host's for an I/O error among the error's sources, other than a missing file, and
/// otherwise the generation's, which is malformed: a payload, a key, or a snapshot that is
/// missing, that is not private to the current user, or that is not one that the store writes.
fn generation_fault(error: Error) -> Fault {
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        if let Some(io) = cause.downcast_ref::<io::Error>()
            && io.kind() != io::ErrorKind::NotFound
        {
            return Fault::new(host_evidence(&error), error);
        }
        source = cause.source();
    }
    Fault::new(Evidence::MalformedGeneration, error)
}

/// Returns the directories through which `path` leads, as the host resolves it, each resolved as
/// [`filesystem::canonicalize`] resolves the paths of an export plan: for each name of the path,
/// `..` included, from the current directory's own for a relative path, the directory in which
/// the host looks it up, which the path before the name resolves to, and, for a name that is a
/// link, the directories through which the link's target leads from there, through at most
/// [`MAX_PATH_LINKS`] links in all.
///
/// The host reaches the template store, and OpenVMM the snapshots in it, by the path that the
/// store names, so a guest that could write in one of these directories could replace a link or
/// a directory of the path, and turn the host's later operations on the store elsewhere, or
/// make the directory unsearchable, and keep the host from the store.
fn path_directories(path: &Path) -> io::Result<Vec<PathBuf>> {
    let mut directories = BTreeSet::new();
    let mut pending = vec![(PathBuf::new(), std::path::absolute(path)?)];
    let mut links = 0;
    while let Some((start, rest)) = pending.pop() {
        let mut prefix = start;
        for component in rest.components() {
            match component {
                Component::Normal(_) => {}
                Component::CurDir | Component::ParentDir => {
                    // Unix looks `.` and `..` up in the directory before them, as it does a name,
                    // so a guest that could change that directory could keep the host from
                    // searching it. Windows resolves them from a path's text, as
                    // `std::path::absolute` did.
                    directories.insert(filesystem::canonicalize(&prefix)?);
                    prefix.push(component);
                    continue;
                }
                Component::RootDir | Component::Prefix(_) => {
                    // The root and a prefix name no entry.
                    prefix.push(component);
                    continue;
                }
            }
            let parent = filesystem::canonicalize(&prefix)?;
            prefix.push(component);
            if fs::symlink_metadata(&prefix)?.file_type().is_symlink() {
                links += 1;
                if links > MAX_PATH_LINKS {
                    return Err(io::Error::other(format!(
                        "{} leads through more than {MAX_PATH_LINKS} links",
                        path.display()
                    )));
                }
                // A link's target that is absolute replaces the directory that it starts from.
                pending.push((parent.clone(), fs::read_link(&prefix)?));
            }
            directories.insert(parent);
        }
    }
    Ok(directories.into_iter().collect())
}

/// Refuses the path of the file of a clone's block device, which messages call `what`, that
/// OpenVMM would not open as the plan checks it: a relative one, which it would resolve against the
/// directory in which it runs, one with a comma or a semicolon, which its block device options
/// would split, and one whose name ends in `.vhd`, `.vhdx`, `.vmgs`, or `.iso`, which it would not
/// attach as a raw image.
fn check_disk_path(path: &PurePath, what: &str) -> Result<()> {
    if !path.is_absolute() {
        return Err(Error::unsupported(format!(
            "the {what} of a clone must be absolute, since OpenVMM would resolve it against the \
             directory in which it runs: {path}"
        )));
    }
    reject_disk_path(path, path.as_str())?;
    reject_container_path(path, path.as_str())
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
) -> Result<(), Fault> {
    let name = generation.generation();
    // The key's contracts are those through which its clones are restored; the runtime's are
    // those through which this build boots and drives the guest that the template holds.
    let runtime = key.runtime();
    if !key.speaks_current_contracts()
        || runtime.boot_contract != contract::BOOT_CONTRACT_VERSION
        || runtime.control_contract != contract::CONTROL_CONTRACT_REVISION
        || runtime.control_session != contract::CONTROL_SESSION_PROTOCOL_VERSION
    {
        return Err(Fault::new(
            Evidence::IncompatibleHost,
            Error::unsupported(format!(
                "the template's generation {name} speaks other contracts than this build, which \
                 restores none of its clones"
            )),
        ));
    }
    // A network that OpenVMM attaches no device for is no NIC, as a capture class's.
    let attached = match request.adapter.network() {
        None => false,
        Some(network) => !network
            .openvmm_arguments()
            .attribute(Evidence::InvalidSettings)?
            .is_empty(),
    };
    if attached != key.has_network() {
        return Err(Fault::new(
            Evidence::InvalidSettings,
            Error::unsupported(if attached {
                format!(
                    "the adapter's plan of a clone attaches a network, but the template's \
                     generation {name} has no NIC"
                )
            } else {
                format!(
                    "the template's generation {name} has a NIC, which its clones restore with \
                     the network of the adapter's plan, but the plan has none"
                )
            }),
        ));
    }
    let memory = request.memory;
    let block = key.memory_block_size();
    if memory < key.memory()
        || memory > key.memory_capacity()
        || !memory.is_multiple_of(MEMORY_BLOCK_SIZE)
        || !memory.is_multiple_of(block)
    {
        return Err(Fault::new(
            Evidence::InvalidTargets,
            Error::unsupported(format!(
                "a clone of the template's generation {name} cannot restore with {memory} bytes \
                 of RAM: its memory target must be whole blocks of {} MiB, and of the template's \
                 memory blocks of {block} bytes, from the template's {} bytes of RAM to its \
                 capacity of {} bytes",
                MEMORY_BLOCK_SIZE / MIB,
                key.memory(),
                key.memory_capacity()
            )),
        ));
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
) -> Result<CheckedDistro, Fault> {
    let path = request.adapter.distro().pure_path();
    let binding = &request.binding;
    let layer = match binding.format() {
        DistroFormat::Erofs => {
            SandboxLayer::new(LayerRole::Distro, path.as_str(), binding.identity())
        }
        DistroFormat::Gpt => {
            SandboxLayer::gpt(LayerRole::Distro, path.as_str(), binding.identity())
        }
    };
    let policy = binding.policy();
    let checked = binding
        .check_with(&layer, request.policy_revision, access.distro)
        .map_err(|error| {
            // The check refuses a stale binding, then one whose verdict rejects the image, before
            // it opens anything.
            let evidence = if binding.version() != distro::BINDING_VERSION
                || policy.revision != request.policy_revision
            {
                Evidence::StaleBinding
            } else if policy.result != PolicyResult::Accepted {
                Evidence::AuthorizationDenied
            } else {
                binding_evidence(&error)
            };
            Fault::new(evidence, error)
        })?;
    let geometry = checked.geometry().map_err(supplied_geometry_fault)?;
    check_geometry(
        geometry,
        key.distro(),
        &format!("the image {path}"),
        "placeholder",
        Mismatch::SUPPLIED,
    )?;
    let identity = identity_of(checked.file()).map_err(|error| {
        Fault::host(Error::io(format!("the image {path} cannot be identified")).with_source(error))
    })?;
    check_at_path(path.as_path(), identity, &format!("the image {path}"))?;
    owned
        .check(identity, "image", path)
        .attribute(Evidence::InvalidSettings)?;
    Ok(checked)
}

/// Checks and claims the scratch image at `path` against `binding`, through `access`, waiting
/// until `deadline` for a claim that another launch holds to be released, and then checks its
/// geometry, against the template's scratch that `key` records, the image at its path, and that
/// it is none of `owned`, the files of the sandbox's store, as [`ClonePlan`] describes, and
/// returns the image that the binding's check opened and claimed.
fn check_scratch(
    path: &PurePath,
    binding: &ScratchBinding,
    key: &TemplateKey,
    owned: &StoreFiles,
    deadline: Instant,
    access: Access<'_>,
) -> Result<CheckedScratch, Fault> {
    let checked = binding
        .check_with(path, deadline, access.scratch)
        .map_err(|error| {
            let evidence = if binding.version() != scratch::SCRATCH_BINDING_VERSION {
                Evidence::StaleBinding
            } else if error.code() == ErrorCode::ScratchClaimed {
                // Another launch holds the image, which is not fresh for this clone.
                Evidence::UnusableScratch
            } else {
                binding_evidence(&error)
            };
            Fault::new(evidence, error)
        })?;
    let geometry = checked.geometry().map_err(supplied_geometry_fault)?;
    check_geometry(
        geometry,
        key.scratch(),
        &format!("the scratch image {path}"),
        "scratch",
        Mismatch::SUPPLIED,
    )?;
    let identity = identity_of(checked.file()).map_err(|error| {
        Fault::host(
            Error::io(format!("the scratch image {path} cannot be identified")).with_source(error),
        )
    })?;
    check_at_path(
        path.as_path(),
        identity,
        &format!("the scratch image {path}"),
    )?;
    owned
        .check(identity, "scratch image", path)
        .attribute(Evidence::InvalidSettings)?;
    Ok(checked)
}

/// Returns the evidence of `error`, with which the check of the binding of a file that the request
/// supplied failed, past the binding's own version and verdict: a file that changed, was replaced,
/// or went since its binding was made, a supplied file of the wrong identity; a path that the
/// check refuses, invalid settings; and any other failure, the host's.
fn binding_evidence(error: &Error) -> Evidence {
    match error.kind() {
        ErrorKind::Unavailable => Evidence::SuppliedIdentity,
        ErrorKind::Unsupported => Evidence::InvalidSettings,
        _ => host_evidence(error),
    }
}

/// Returns the fault of `error`, with which the geometry of a file that the request supplied
/// cannot be read: a geometry that OpenVMM refuses, a supplied file of the wrong geometry, and any
/// other failure, the host's.
fn supplied_geometry_fault(error: Error) -> Fault {
    match error.kind() {
        ErrorKind::Unsupported => Fault::new(Evidence::SuppliedGeometry, error),
        _ => Fault::host(error),
    }
}

/// The regular files that the store of a sandbox owns, with their identities, which no image or
/// scratch image of a clone may be.
struct StoreFiles(Vec<(PurePath, (u64, u128))>);

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
            // The sandbox's path, which its lease names, is valid Unicode, and so are the names
            // that the store owns.
            let shown = PurePath::native(&entry.to_string_lossy());
            let absent = |error: &io::Error| {
                error.kind() == io::ErrorKind::NotFound || was_removed(&entry, error)
            };
            let failed = |error: io::Error| {
                Error::io(format!(
                    "cannot examine {shown}, a file of the sandbox's store"
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
                    "{shown} is a link or a directory, which the sandbox's store never creates \
                     there"
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
    fn check(&self, identity: (u64, u128), what: &str, path: &PurePath) -> Result<()> {
        match self.0.iter().find(|(_, owned)| *owned == identity) {
            None => Ok(()),
            Some((shown, _)) => Err(Error::unsupported(format!(
                "the {what} {path} is {shown}, a file of the sandbox's store, which its \
                 transitions replace or remove"
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
fn create_scratch(
    locked: &LockedStore,
    key: &TemplateKey,
    access: Access<'_>,
) -> Result<PathBuf, Fault> {
    let path = locked.clone_scratch_path();
    // The sandbox's path, which its lease names, is valid Unicode. The host chose it.
    let pure = PurePath::native(&path.to_string_lossy());
    reject_disk_path(&pure, pure.as_str()).attribute(Evidence::HostFailure)?;
    let failed = |error: io::Error| {
        Fault::host(Error::io(format!("cannot create the {SPARSE_WHAT} {pure}")).with_source(error))
    };
    let created = (|| {
        let file = (access.create)(&path).map_err(&failed)?;
        let scratch = key.scratch();
        file.set_len(scratch.length).map_err(&failed)?;
        let geometry =
            block_geometry(&file, &pure, SPARSE_WHAT).map_err(|error| match error.kind() {
                // The sandbox's filesystem presents a geometry that OpenVMM refuses.
                ErrorKind::Unsupported => Fault::new(Evidence::IncompatibleHost, error),
                _ => Fault::host(error),
            })?;
        check_geometry(
            geometry,
            scratch,
            &format!("the {SPARSE_WHAT} {pure}, in the sandbox's filesystem,"),
            "scratch",
            Mismatch::HOST,
        )?;
        let identity = identity_of(&file).map_err(&failed)?;
        drop(file);
        // The host made the file, so what its path shows of it is the host's.
        check_at_path(&path, identity, &format!("the {SPARSE_WHAT} {pure}")).map_err(|fault| {
            let (_, message, source) = fault.error.into_parts();
            let error = Error::io(message);
            Fault::host(match source {
                Some(source) => error.with_source(source),
                None => error,
            })
        })
    })();
    match created {
        Ok(()) => Ok(path),
        Err(fault) => {
            let _ = remove_if_present(&path);
            Err(fault)
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

/// The evidence of a file of a clone's block device whose geometry is not the one that the
/// template's device presented.
#[derive(Clone, Copy)]
struct Mismatch {
    /// The evidence of another length.
    length: Evidence,
    /// The evidence of other block sizes.
    blocks: Evidence,
}

impl Mismatch {
    /// A file that the request supplied.
    const SUPPLIED: Self = Self {
        length: Evidence::SuppliedLength,
        blocks: Evidence::SuppliedGeometry,
    };

    /// The sparse file of an upper in RAM, which the plan sizes as the template's scratch, and
    /// whose block sizes the sandbox's filesystem presents: the host's.
    const HOST: Self = Self {
        length: Evidence::IncompatibleHost,
        blocks: Evidence::IncompatibleHost,
    };
}

/// Refuses `found`, the geometry of the file of a clone's block device, which messages call
/// `what`, unless it is `expected`, the geometry that the template's `device` presented, with the
/// evidence that `mismatch` gives.
fn check_geometry(
    found: BlockGeometry,
    expected: BlockGeometry,
    what: &str,
    device: &str,
    mismatch: Mismatch,
) -> Result<(), Fault> {
    if found.length != expected.length {
        return Err(Fault::new(
            mismatch.length,
            Error::unsupported(format!(
                "{what} is {} bytes long, not the {} bytes of the template's {device}",
                found.length, expected.length
            )),
        ));
    }
    if found != expected {
        return Err(Fault::new(
            mismatch.blocks,
            Error::unsupported(format!(
                "{what} presents logical blocks of {} bytes and physical blocks of {} bytes, but \
                 the template's {device} presented {} and {} bytes",
                found.logical_block_size,
                found.physical_block_size,
                expected.logical_block_size,
                expected.physical_block_size
            )),
        ));
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

/// Checks the file at `path`, which messages call `what`, as OpenVMM will open it, by its path: a
/// regular file, not a link, nor on Windows any other reparse point, whose identity is
/// `expected`, the identity of the file that a check found, and that no one but the current user,
/// or an administrator, may change.
///
/// Fails with [`ErrorKind::Unavailable`] and [`Evidence::SuppliedIdentity`] for a path that no
/// longer names that file: one whose file went, a component of which is no longer a directory,
/// that the current user may no longer open, or that names a link, a file that is not regular, or
/// another file, and for a file that others may change; and with [`ErrorKind::Io`] and the host's
/// evidence if the file cannot be opened at its path otherwise, as when the host runs out of
/// descriptors, or if the file that it opened cannot be identified or examined.
fn check_at_path(path: &Path, expected: (u64, u128), what: &str) -> Result<(), Fault> {
    let refused = |problem: String| {
        Fault::new(
            Evidence::SuppliedIdentity,
            Error::unavailable(format!("{what} {problem}")),
        )
    };
    let failed = |action: &str, error: io::Error| {
        Fault::host(Error::io(format!("{what} cannot be {action}")).with_source(error))
    };
    let file = open_regular(path).map_err(|error| {
        if path_refused(&error) {
            refused(format!("cannot be used: {error}"))
        } else {
            failed("opened", error)
        }
    })?;
    let identity = identity_of(&file).map_err(|error| failed("identified", error))?;
    if identity != expected {
        return Err(refused(
            "changed while it was checked: its path names another file".to_owned(),
        ));
    }
    if let Some(problem) =
        platform::foreign_writer_problem(&file).map_err(|error| failed("checked", error))?
    {
        return Err(refused(problem));
    }
    Ok(())
}

/// Returns whether `error`, with which [`open_regular`] failed to open a file that a request
/// supplied at its path, says that the path no longer names the file that a check found: the
/// file went, a component of the path is no longer a directory, the current user may no longer
/// open it, or the open refused a link or a file that is not regular, a failure that the system
/// does not report. Any other failure is the host's.
fn path_refused(error: &io::Error) -> bool {
    error.raw_os_error().is_none()
        || matches!(
            error.kind(),
            io::ErrorKind::NotFound
                | io::ErrorKind::NotADirectory
                | io::ErrorKind::PermissionDenied
        )
}
