//! The plan of a clone, [`ClonePlan`]: the generation of a template that a clone leased, with the
//! clone's own image, upper, processor and memory targets, and export, which the plan checks
//! against the generation before the clone's launch, so that a request that the template cannot
//! serve fails before any VMM starts; and the clone's launch, [`ClonePlan::launch`], OpenVMM's
//! restore of the generation's snapshot with what the plan checked.

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

use self::export::CloneExport;
use self::failure::{Attribute, Fault, host_evidence};
use super::key::TemplateKey;
use super::manifest::{BlockGeometry, PROCESSOR_COUNTS};
use super::plan::MEMORY_BLOCK_SIZE;
use super::store::{Acquired, Generation, Lease, Store};
use crate::activation::{ActivationRecord, Journal, OverlayMode, WorkloadView};
use crate::contract;
use crate::error::{Error, ErrorCode, ErrorKind, Result};
use crate::host::filesystem::{self, ExportPlan};
use crate::host::instance::generate_capability;
use crate::host::paths;
use crate::host::platform;
use crate::host::sandbox::distro::{
    self, CheckedDistro, DistroBinding, DistroFormat, PolicyResult,
};
use crate::host::sandbox::plan::bound_image;
use crate::host::sandbox::scratch::{self, CheckedScratch, ScratchBinding};
use crate::host::sandbox::{
    LayerRole, SandboxLayer, SandboxPlan, reject_container_path, reject_disk_path,
};
use crate::host::store::LockedStore;
use crate::host::store::regular::{identity_of, is_refused, open_regular, was_removed};
use crate::protocol::{CAPABILITY_LEN, HostMap};

/// Bytes in a mebibyte, the unit in which messages give the size of memory blocks.
const MIB: u64 = 1024 * 1024;

/// The most links that [`path_directories`] follows in a path, more than a Linux host, 40, or a
/// Windows host, 63, follows in one.
const MAX_PATH_LINKS: usize = 64;

/// What the clone of a sandbox restores of the generation of a template that it leased: the
/// adapter's plan of the clone's image and upper, the binding of the image, the binding of the
/// scratch image of an upper on one, the export of the host paths that the clone shares with its
/// root, if any, and the processor and memory targets, which [`ClonePlan::new`] checks against the
/// generation, and the settings of the clone's activation that the template does not fix, which
/// the clone's [activation record](ClonePlan::activation) names: the workload's view of its VM,
/// the overlay's mode, the journal policy of a scratch image, and the bound of a GPT disk's first
/// partition.
#[derive(Debug, Clone)]
pub struct CloneRequest {
    /// The adapter's plan, which [`SandboxPlan::adapter`] makes, of the clone's image, its
    /// `distro`, an EROFS layer or a GPT disk, over its upper: in RAM, a tmpfs, as
    /// [`SandboxUpper::RAM`](crate::host::sandbox::SandboxUpper::RAM) plans it, or on the
    /// caller's scratch image. Its network is the clone's own, whose policy the restore enforces
    /// on the template's NIC in place of the template's complete deny-all policy, at the guest's
    /// address that the NIC has, [`TemplateKey::guest_network`], or for a template without a NIC
    /// none, or one that lets nothing through, which attaches none either.
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
    /// dormant. The restore binds the export to the slot as a cold launch attaches it, an
    /// aggregate of the plan's directories and files, as [`ClonePlan`] describes.
    pub export: Option<ExportPlan>,
    /// The processor target: the processors online in the clone's VM once restored, a prefix of
    /// the template's processor capacity, of which the restore brings online those beyond the
    /// template's processors online at boot.
    pub processors: u32,
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

/// The upper of a clone's plan, which decides how the clone's restore attaches the template's
/// `scratch` block device.
#[derive(Debug)]
pub enum CloneUpper {
    /// An upper in RAM, a tmpfs, which leaves the `scratch` block device unused: the restore
    /// declares the device absent, and OpenVMM attaches it with no file, at the geometry that the
    /// template recorded, as [`ClonePlan::launch`] describes, so the plan creates no file for it.
    Tmpfs,
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
/// `scratch`, at the geometries that the template recorded, as OpenVMM presents them: the
/// `distro`, the clone's image, at the placeholder's, the capacity of every clone's image, over an
/// image that may be shorter, whose blocks past its end read as zeros, and the `scratch` at the
/// template's scratch, over the clone's scratch image, of exactly that geometry, for an upper on
/// one, or with no file, for an upper in RAM, whose restore declares it absent. OpenVMM opens the
/// clone's files itself, by their paths, as [Block devices by path](Store#block-devices-by-path)
/// describes. The plan checks each of the clone's inputs as OpenVMM will find it, and holds what
/// the checks opened:
///
/// - The image, the clone's `distro`, an EROFS layer or a GPT disk, as its binding names it, must
///   pass the check of its binding, [`DistroBinding::check`], and fit the capacity of the
///   template's placeholder, as its key records it, [`TemplateKey::distro`]: no longer than the
///   placeholder, with the same logical and physical block sizes, which
///   [`CheckedDistro::geometry`] reads through the checked file, whatever the placeholder's
///   format. On Linux, a file's physical block size is the I/O block size of its filesystem, so
///   the image must lie on a filesystem of the placeholder's. At its path, it must be a regular
///   file, not a link, nor on Windows any other reparse point, that is the file that the check
///   found, that belongs to the current user or to an administrator, and that no one else may
///   change. The plan holds the file that the check opened, for reading alone. Nothing resizes,
///   pads, or writes the image. As at a cold launch, the guest reads and checks a GPT disk's
///   partition table, with `nvx::gpt`, before it assembles the root, so the plan reads none of
///   it: of a disk shorter than the capacity, `nvx::gpt` requires only that its backup header lie
///   inside the capacity, as it does at the disk's end.
/// - Neither the image nor a scratch image may be a file that the sandbox's store owns, by any
///   path, through a link among its directories, say, or a hard link: the store's transitions
///   replace or remove its files, as a launch replaces its log and the clone's activation record,
///   and a release removes the clone binding. The store creates nothing but regular files under
///   its names, and its control socket, so the plan also refuses a link or a directory under one
///   of them, through which the path of an image could pass.
/// - An upper in RAM, a tmpfs, leaves the `scratch` block device unused, which the restore
///   attaches all the same, since the template's guest negotiated it: the restore declares it
///   absent, and OpenVMM attaches it with no file, as [`launch`](Self::launch) describes. The plan
///   creates nothing for it.
/// - An upper on the caller's scratch image must pass the check of its binding,
///   [`ScratchBinding::check`], which checks the image's identity, kind, and length and claims it
///   for the clone, and present the geometry of the template's scratch, its length included,
///   which [`CheckedScratch::geometry`] reads through the checked file. At its path, it must be a
///   regular file, not a link, nor on Windows any other reparse point, that is the file that the
///   check found, and that no one else may change. Nor may it be a file of the snapshot of any of
///   the store's generations, by any path, through a hard link, say: every clone of a generation
///   restores its snapshot in place, while its guest writes its scratch image. So, once claimed,
///   it must be outside the store's directory, wherever the links on its path lead: the store's
///   generations hold their snapshots there, and its captures write theirs there, which their
///   publications make generations' by renaming directories within it, so a file in the store
///   could be, or become as the plan examines it, a file of a snapshot, while nothing that the
///   host does moves a file into the store's directory. And it must have a single name: another,
///   a hard link, could make it a file of a snapshot, of this store or of another, that its
///   location does not show. These checks open nothing in the store, however many generations it
///   holds. They resolve the aliases that the file system makes, links, junctions, directories
///   held open, and other names, but not a mount or a network share that exposes the store's
///   directories elsewhere: a store's directories must not be exposed that way. The plan holds
///   the checked image, and with it the claim. Nothing reads, formats, or copies the image.
/// - The processor target must be 1, 2, 4, or 8, from the template's processors online at boot to
///   its processor capacity, as its key records them, [`TemplateKey::boot_processors`] and
///   [`TemplateKey::processors`]. A template whose processors are all online at boot records none
///   online at boot, and serves the target of its capacity alone.
/// - The memory target must be whole blocks of [`MEMORY_BLOCK_SIZE`], and of the template's own
///   memory blocks, from the template's RAM to its RAM capacity.
/// - The network of the adapter's plan must attach a NIC, as
///   [`SandboxNetwork::openvmm_arguments`](crate::host::sandbox::SandboxNetwork::openvmm_arguments)
///   lowers it, if and only if the template has one, at the guest's address of the template's NIC,
///   which its key records, [`TemplateKey::guest_network`]. A template captures its NIC under the
///   complete deny-all policy, which the restore replaces with the policy of the clone's network,
///   so a template serves the clones of every network policy at its guest's address: egress
///   rules, a proxy, ingress, host-loopback access, and host-loopback forwards alike.
/// - The export, if the request has one, must be a plan that [`filesystem::plan`] could have made
/// - The export, if the request has one, must be a plan that [`filesystem::plan`] could have made
///   for the container root, as [`ExportPlan::check`] finds it: an aggregate of up to
///   [`MAX_CHILDREN`](crate::host::filesystem::MAX_CHILDREN) host directories and files, each
///   read-only or read-write, with OpenVMM's hidden, re-exposed, and writable paths, and its
///   owner. A template's filesystem slot is dormant, and the restore of a template, a platform
///   snapshot, binds the aggregate to it as a cold launch attaches one, so the sources of the
///   plan's binds name their paths by their child's index, as the mapping table,
///   [`table`](Self::table), sends them, as at a cold launch. None of its directories and files
///   may lie inside the template store's directory, nor any of its directories hold it, by the
///   paths that the plan and the store's directory resolve to, so that no guest reaches the
///   payload of a generation through it, nor may the export let the guest write in a directory
///   through which the store's path leads, links and `..` included, since the host reaches the
///   store, and OpenVMM its snapshots, by that path. Nor may one of its directories be, or hold,
///   the directory of the clone's sandbox, which the lease names by its resolved path, and in
///   which the host writes the clone's records, which the guest could otherwise read or change,
///   or turn elsewhere, into a leased snapshot, say, nor may one of its files lie in that
///   directory itself, where it may be one of those records, though a directory inside the
///   sandbox's, among the caller's files, may be exported, as may a file in it. These checks
///   compare paths, so an alias of the
///   store that an administrator configures is outside them, as the directories above the store
///   are: a bind mount or a mount point of the store, or of a directory above or inside it, a
///   loopback share, or a hard link of a payload. Whoever names an export must not name such an
///   alias, nor a directory that holds one. The launch checks the plan's host objects again, with
///   [`ExportPlan::verify`], before it restores the clone, but OpenVMM resolves the paths of an
///   export again at each launch and restore, so the directories above each of the export's
///   directories and files must be ones that no guest may change, as those above the clone's
///   block devices must, which [Block devices by path](Store#block-devices-by-path) describes:
///   whoever names an export must not name one inside a directory that a writable export of
///   another sandbox or clone holds, whose guest could replace the directory with a link, and
///   turn the export elsewhere, to a leased payload, say. A request without an export leaves the
///   slot dormant.
/// - The activation record, which the clone's guest applies in place of the configuration that
///   the template captured, names the restore, with a fresh transaction ID, the clone's image,
///   upper, export, and settings, its network, and the resources that the clone must
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
///     processors: 2,
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
    /// The processor target.
    processors: u32,
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
    export: Option<CloneExport>,
}

/// How a plan reaches the clone's files: as the checks of their bindings reach them, or, in tests,
/// through opens that read no content, or that reach files on filesystems of other block sizes.
#[derive(Clone, Copy)]
struct Access<'a> {
    /// How the check of the image's binding reaches the image.
    distro: distro::Access<'a>,
    /// How the check of the scratch image's binding reaches the image.
    scratch: scratch::Access<'a>,
}

impl Access<'static> {
    /// The access of every plan outside tests.
    const CHECKED: Self = Self {
        distro: distro::Access::GUARDED,
        scratch: scratch::Access::WRITABLE,
    };
}

impl ClonePlan {
    /// Plans the clone of the sandbox of `acquired`, which leased a generation of `store`, as
    /// `request` asks, waiting until `deadline` for a claim of its scratch image that another
    /// launch holds to be released.
    ///
    /// The plan is made under the lock that acquired the lease, which `acquired` holds, before the
    /// launch that the lease reserves, [`Lease::launch`], which the caller then runs under the
    /// same lock, with [`launch`](Self::launch), while the sandbox records no VM and no launch. It
    /// takes `acquired` exclusively while it plans, as the transitions of a sandbox's store take
    /// its lock, since it checks the files of the sandbox's store, and borrows nothing of it once
    /// it returns. It creates nothing in the sandbox's directory. It reads the generation's key,
    /// [`Store::read_key`], without computing its identity again, and checks the generation's
    /// snapshot, [`Store::snapshot_dir`], then checks the clone's files, as the [type](Self)
    /// describes.
    ///
    /// Fails with a [`CloneFailure`] at [`Stage::Plan`], and holds nothing. The failure's record
    /// holds the evidence of the check that failed, as below, no transaction, and no VM,
    /// [`Teardown::NoVm`], and the plan records it nowhere: the caller holds it, and releases the
    /// lease. The evidence of a failure of the host's own operations is
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
    ///   finds it, such as one that maps paths but exports no directory or file, exports some but
    ///   maps no path, or exports more directories and files than OpenVMM attaches,
    ///   [`Evidence::InvalidMappings`];
    /// - with [`ErrorKind::Other`] for a lease of another store, and a lock that is not the one
    ///   that acquired the lease, before the launch that it reserves began,
    ///   [`Evidence::HostFailure`];
    /// - with [`ErrorKind::Unsupported`] for an export of which a directory or a file, by the path
    ///   that its plan resolved, lies inside the template store's directory, [`Store::dir`], by
    ///   the path that it resolves to, or of which a directory holds it, or is or holds the
    ///   directory of the clone's sandbox, [`Lease::sandbox`], or of which a file lies in that
    ///   directory itself, or that lets the guest write in a directory through which the store's
    ///   path leads, as the host resolves it, links and `..` included, which the guest could then
    ///   turn elsewhere, or keep the host from following, [`Evidence::InvalidMappings`], and with
    ///   [`ErrorKind::Io`] if the store's directory, or a directory through which its path leads,
    ///   cannot be resolved, the host's evidence;
    /// - with [`ErrorKind::NotFound`] if the store no longer indexes the leased generation, and as
    ///   [`Store::index`] fails, the host's evidence; with [`ErrorKind::Io`] if it indexes it with
    ///   another key than the lease names, [`Evidence::MalformedGeneration`]; and as
    ///   [`Store::read_key`] and [`Store::snapshot_dir`] fail, once the store's directory of
    ///   generations passed their check, whose failure is the host's: the host's evidence for an
    ///   I/O error, [`Evidence::IncompatibleHost`] for a key of another encoding version than this
    ///   build reads, with [`ErrorCode::FormatUnsupported`], which a build that reads it may
    ///   serve, and [`Evidence::MalformedGeneration`] for a generation whose payload, key, or
    ///   snapshot is missing, is not private to the current user, or is malformed, as a key that
    ///   [`Store::read_key`] refuses for what it is: a link, a file that is not a regular file, or
    ///   one that holds more than a key may;
    /// - with [`ErrorKind::Unsupported`] for a request that the template cannot serve: a template
    ///   whose key names other contracts than this build speaks, the boot and control contracts
    ///   of the runtime that captured it included, [`Evidence::IncompatibleHost`]; a plan whose
    ///   network does not lower to OpenVMM's arguments, as
    ///   [`SandboxNetwork::openvmm_arguments`](crate::host::sandbox::SandboxNetwork::openvmm_arguments)
    ///   finds it, a plan with a network for a template without a NIC, or without one for a
    ///   template with a NIC, or one at another guest's address than the template's NIC, as its
    ///   key records it, [`TemplateKey::guest_network`], [`Evidence::InvalidSettings`]; and a
    ///   processor target other than 1, 2, 4, or 8, or
    ///   outside the template's processors online at boot and its processor capacity, and a
    ///   memory target that is not whole blocks of [`MEMORY_BLOCK_SIZE`] and of the template's
    ///   memory blocks, from the template's RAM to its RAM capacity, [`Evidence::InvalidTargets`];
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
    ///   [`ErrorKind::Unsupported`] for an image of other block sizes than the template's
    ///   placeholder, or of a geometry that OpenVMM refuses, [`Evidence::SuppliedGeometry`], or
    ///   longer than the placeholder, the capacity of the template's clones' images,
    ///   [`Evidence::SuppliedLength`]; and with [`ErrorKind::Unsupported`] for an image that is a
    ///   file of the sandbox's store, [`Evidence::InvalidSettings`];
    /// - for an upper on a scratch image, with [`ErrorKind::Io`] if the image's location cannot be
    ///   compared with the template store's directory, or its names cannot be counted, the host's
    ///   evidence, and as [`ScratchBinding::check`] and [`CheckedScratch::geometry`] fail: with
    ///   [`ErrorKind::TimedOut`] and [`ErrorCode::ScratchClaimed`] for an image that another
    ///   launch still claims, [`Evidence::UnusableScratch`]; with [`ErrorKind::Unavailable`] for an
    ///   image that changed, was replaced, or went since its binding was made, or that is not, at
    ///   its path, the regular file that the check found, of the current user or an administrator,
    ///   and that no one else may change, [`Evidence::SuppliedIdentity`], and with
    ///   [`ErrorKind::Io`] if the image cannot be opened at its path otherwise, or identified or
    ///   examined once open, the host's evidence; with [`ErrorKind::Unsupported`] for an image of
    ///   another length than the template's scratch, [`Evidence::SuppliedLength`], or of other
    ///   block sizes, [`Evidence::SuppliedGeometry`]; and with [`ErrorKind::Unsupported`] for an
    ///   image that is a file of the sandbox's store, or that is in the template store's
    ///   directory, as the files of its snapshots are, or has another name, or whose path is not
    ///   Unicode, [`Evidence::InvalidSettings`].
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
            // which it leads, in which the guest must not write. The lease names the sandbox's
            // directory, in which the host writes the clone's records, by its resolved path.
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
                .check_outside(dir, &resolved, &through, lease.sandbox())
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
        let upper = match (request.adapter.upper().scratch_path(), &request.scratch) {
            (Some(path), Some(binding)) => CloneUpper::Scratch(check_scratch(
                path,
                binding,
                &key,
                (&owned, store),
                deadline,
                access,
            )?),
            _ => CloneUpper::Tmpfs,
        };
        let devices = devices_of(request);
        Ok(Self {
            lease: lease.clone(),
            generation,
            key,
            snapshot,
            image,
            upper,
            processors: request.processors,
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

    /// Returns the clone's upper, which decides how its restore attaches the `scratch` block
    /// device.
    pub fn upper(&self) -> &CloneUpper {
        &self.upper
    }

    /// Returns the file of the clone's `scratch` block device, which OpenVMM opens at this path:
    /// the caller's scratch image, or `None` for an upper in RAM, whose restore declares the
    /// device absent.
    pub fn scratch_path(&self) -> Option<&Path> {
        match &self.upper {
            CloneUpper::Tmpfs => None,
            CloneUpper::Scratch(checked) => Some(checked.path()),
        }
    }

    /// Returns the processor target: the processors online in the clone's VM once restored.
    pub fn processors(&self) -> u32 {
        self.processors
    }

    /// Returns the memory target, in bytes.
    pub fn memory(&self) -> u64 {
        self.memory
    }

    /// Returns the export plan of the host paths that the clone shares with its root, which its
    /// restore binds to the template's dormant filesystem slot, or `None` for a clone that shares
    /// none, whose slot stays dormant.
    pub fn export(&self) -> Option<&ExportPlan> {
        self.export.as_ref().map(CloneExport::plan)
    }

    /// Returns the clone's mapping table, which its restore sequence sends once the clone is
    /// ready, before its sandbox starts, as [`Activated::start`] describes: the binds of the
    /// export plan, in their order, each from its path in the export, by its child's index, as a
    /// cold launch's guest receives them, [`ExportPlan::guest_table`]. A clone without an export
    /// has an empty table, and the sequence sends none.
    pub fn table(&self) -> Vec<HostMap> {
        self.export
            .as_ref()
            .map_or_else(Vec::new, CloneExport::table)
    }

    /// Returns the clone's activation record, which its guest applies in place of the
    /// configuration that the template captured, and whose restore transaction ID, as
    /// [`ReadyActivation::of`](crate::activation::ReadyActivation::of) returns it, the guest's
    /// Ready reports.
    ///
    /// The record holds the restore section, with the restore's transaction ID, 16 random bytes
    /// that the plan drew, and the template that the [lease](Lease::template) names; the image, in
    /// the format that its binding names, with the identity of its binding: an EROFS layer, with
    /// the UUID that the guest expects of its filesystem, or a GPT disk, with the GUID that the
    /// guest expects of the disk, each if the adapter's plan names one, and the request's bound of
    /// the disk's first partition; the upper, a tmpfs, or the scratch image, with the identity of
    /// its binding and the request's journal policy; the request's overlay mode and workload view;
    /// the export, if the request has one, an aggregate, with the identity of its plan, and the
    /// number of its mappings, the binds of the plan, from 1 to
    /// [`MAX_MAPPINGS`](crate::activation::MAX_MAPPINGS), or no export and no mappings; the
    /// clone's network, if the template has a NIC, with the identity of the network, whose policy
    /// the restore enforces on the NIC, the DNS server that OpenVMM serves the guest at a gateway
    /// under that policy, if any, whether the guest publishes its listeners, as it does under a
    /// policy that allows ingress or host-loopback access both ways, and the URL of the policy's
    /// proxy as the guest sees it, if any; and the processor target, which the restore brings
    /// online, and the memory target, as the resources that the clone must have.
    ///
    /// The identity of a binding, of an export plan, or of a network is the SHA-256 digest of a
    /// domain of its kind and of its serde form, a few hundred bytes of metadata: the plan reads
    /// no content to compute it.
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
            .field("processors", &self.processors)
            .field("memory", &self.memory)
            .field("activation", &self.activation)
            .field("export", &self.export())
            .finish_non_exhaustive()
    }
}

/// Returns the devices of the clone of `request`: the adapter's plan of its image over its upper,
/// which renders the `scratch` block device of an upper on a scratch image alone, since the
/// restore declares that of an upper in RAM absent, and of its network, whose policy the restore
/// enforces on the template's NIC.
fn devices_of(request: &CloneRequest) -> SandboxPlan {
    let adapter = &request.adapter;
    SandboxPlan::adapter(adapter.distro().clone(), adapter.upper().clone())
        .with_network(adapter.network().cloned())
}

/// Refuses a request that no clone takes, as [`ClonePlan::new`] describes, without reading or
/// opening anything, and returns the request's export, if it has one, which the checks admitted.
fn check_request(request: &CloneRequest) -> Result<Option<CloneExport>, Fault> {
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
    check_disk_path(distro.path(), "image").attribute(Evidence::InvalidSettings)?;
    match (adapter.upper().scratch_path(), &request.scratch) {
        (Some(path), Some(_)) => {
            check_disk_path(path, "scratch image").attribute(Evidence::InvalidSettings)?;
        }
        (None, None) => {}
        (Some(path), None) => {
            return Err(Fault::new(
                Evidence::InvalidSettings,
                Error::unsupported(format!(
                    "the upper of a clone on the scratch image {} needs the image's scratch \
                     binding",
                    path.display()
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
        .map(CloneExport::new)
        .transpose()
        .attribute(Evidence::InvalidMappings)
}

/// Returns the fault of `error`, with which the template store's read of the key or of the
/// snapshot of the leased generation failed, once the store's directory of generations passed its
/// check: the host's, which cannot restore the template, for a key of another encoding version
/// than this build reads, as [`TemplateKey::from_bytes`] refuses it with
/// [`ErrorCode::FormatUnsupported`], since a build that reads it may serve the generation; the
/// generation's, which is malformed, for a payload, a key, or a snapshot that is missing, that is
/// not private to the current user, or that is not one that the store writes, such as a key that
/// the read refuses for what it is, as [`is_refused`] tells: a link, a file that is not a regular
/// file, or one that holds more than a key may; and the host's for any other I/O error among the
/// error's sources.
fn generation_fault(error: Error) -> Fault {
    if error.code() == ErrorCode::FormatUnsupported {
        return Fault::new(Evidence::IncompatibleHost, error);
    }
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        if let Some(io) = cause.downcast_ref::<io::Error>()
            && io.kind() != io::ErrorKind::NotFound
            && !is_refused(io)
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
    // A template captures its NIC under the complete deny-all policy, which every restore replaces
    // with the policy of its clone's network, so it serves the clones of every network at its
    // guest's address, or, without a NIC, those of none, or of one that lets nothing through,
    // which attaches no NIC either.
    let requested = match request.adapter.network() {
        None => None,
        Some(network) => {
            let attached = !network
                .openvmm_arguments()
                .attribute(Evidence::InvalidSettings)?
                .is_empty();
            attached.then(|| network.guest_network())
        }
    };
    let problem = match (requested, key.guest_network()) {
        (Some(_), None) => Some(format!(
            "the adapter's plan of a clone attaches a network, but the template's generation \
             {name} has no NIC"
        )),
        (None, Some(_)) => Some(format!(
            "the template's generation {name} has a NIC, which its clones restore with the \
             network of the adapter's plan, but the plan has none"
        )),
        (Some(requested), Some(template)) if requested != template => Some(format!(
            "the adapter's plan of a clone attaches a network at {requested}, but the NIC of the \
             template's generation {name} is at {template}: a template serves the clones of its \
             own guest's address alone"
        )),
        _ => None,
    };
    if let Some(problem) = problem {
        return Err(Fault::new(
            Evidence::InvalidSettings,
            Error::unsupported(problem),
        ));
    }
    let processors = request.processors;
    let capacity = key.processors();
    // A template that records no processors online at boot has all of them online, and a restore
    // brings none online.
    let boot = match key.boot_processors() {
        0 => capacity,
        boot => boot,
    };
    if !PROCESSOR_COUNTS.contains(&processors) || processors < boot || processors > capacity {
        return Err(Fault::new(
            Evidence::InvalidTargets,
            Error::unsupported(format!(
                "a clone of the template's generation {name} cannot restore with a processor \
                 target of {processors}: a target must be 1, 2, 4, or 8, from the template's \
                 processors online at boot, {boot}, to its capacity, {capacity}"
            )),
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
/// against the capacity of the placeholder that `key` records, the image at its path, and that it
/// is none of `owned`, the files of the sandbox's store, as [`ClonePlan`] describes, and returns
/// the file that the binding's check opened.
fn check_image(
    request: &CloneRequest,
    key: &TemplateKey,
    owned: &StoreFiles,
    access: Access<'_>,
) -> Result<CheckedDistro, Fault> {
    let path = request.adapter.distro().path();
    let shown = path.display();
    let text = path.to_str().ok_or_else(|| {
        Fault::new(
            Evidence::InvalidSettings,
            Error::unavailable(format!(
                "the image {shown} has a path that is not valid Unicode"
            )),
        )
    })?;
    let binding = &request.binding;
    let layer = match binding.format() {
        DistroFormat::Erofs => SandboxLayer::new(LayerRole::Distro, text, binding.identity()),
        DistroFormat::Gpt => SandboxLayer::gpt(LayerRole::Distro, text, binding.identity()),
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
    check_capacity(geometry, key.distro(), &format!("the image {shown}"))?;
    let identity = identity_of(checked.file()).map_err(|error| {
        Fault::host(Error::io(format!("the image {shown} cannot be identified")).with_source(error))
    })?;
    check_at_path(path, identity, &format!("the image {shown}"))?;
    owned
        .check(identity, "image", path)
        .attribute(Evidence::InvalidSettings)?;
    Ok(checked)
}

/// Checks and claims the scratch image at `path` against `binding`, through `access`, waiting
/// until `deadline` for a claim that another launch holds to be released, and then checks its
/// geometry, against the template's scratch that `key` records, the image at its path, that it
/// is none of `owned`, the files of the sandbox's store, that it is outside the directory of
/// `store`, the template store, and that it has no other name, as [`ClonePlan`] describes, and
/// returns the image that the binding's check opened and claimed.
fn check_scratch(
    path: &Path,
    binding: &ScratchBinding,
    key: &TemplateKey,
    (owned, store): (&StoreFiles, &Store),
    deadline: Instant,
    access: Access<'_>,
) -> Result<CheckedScratch, Fault> {
    let shown = path.display();
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
        &format!("the scratch image {shown}"),
        "scratch",
    )?;
    let identity = identity_of(checked.file()).map_err(|error| {
        Fault::host(
            Error::io(format!("the scratch image {shown} cannot be identified")).with_source(error),
        )
    })?;
    check_at_path(path, identity, &format!("the scratch image {shown}"))?;
    owned
        .check(identity, "scratch image", path)
        .attribute(Evidence::InvalidSettings)?;
    check_outside_store(&checked, store, path)?;
    check_single_name(&checked, path)?;
    Ok(checked)
}

/// Refuses the clone's scratch image at `path`, which `checked` holds open and claims, if it is in
/// the directory of `store`, the template store, where it is now, every link on the way to it
/// resolved: the store's generations hold their snapshots there, which every clone of a
/// generation restores in place, and its captures write theirs there, which their publications
/// make generations' by renaming directories within it, so a file in the store could be, or
/// become as the plan examines it, a file of such a snapshot. Nothing that the host does moves a
/// file into the store's directory, so one outside it stays outside it. The location resolves the
/// aliases that the file system makes, links and junctions on the way and directories held open,
/// but not a mount or a network share that exposes the store's directories elsewhere, which
/// [`ClonePlan`] leaves to the system's administration. The request supplied the image, so its
/// refusal is invalid settings, while the failure to locate it is the host's.
fn check_outside_store(checked: &CheckedScratch, store: &Store, path: &Path) -> Result<(), Fault> {
    let failed = |error: io::Error| {
        Fault::host(
            Error::io(format!(
                "cannot compare the location of the scratch image {} with the template store in \
                 {}",
                path.display(),
                store.dir().display()
            ))
            .with_source(error),
        )
    };
    let location = location_of(checked.file()).map_err(failed)?;
    if is_within(&location, store.dir()).map_err(failed)? {
        return Err(Fault::new(
            Evidence::InvalidSettings,
            Error::unsupported(format!(
                "the scratch image {} is {}, in the directory of the template store {}, where its \
                 captures write the snapshots that every clone of a generation restores in \
                 place: a clone's scratch image must be outside it",
                path.display(),
                location.display(),
                store.dir().display()
            )),
        ));
    }
    Ok(())
}

/// Returns whether the directory `dir` is one of those above `location`, by their identities, past
/// those that went from their paths meanwhile, as a publication renames a capture's transaction,
/// which `dir`, the store's own directory, outlives.
fn is_within(location: &Path, dir: &Path) -> io::Result<bool> {
    let dir = platform::file_id(dir)?;
    for ancestor in location.ancestors().skip(1) {
        match platform::file_id(ancestor) {
            Ok(identity) if identity == dir => return Ok(true),
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

/// Returns where the open `file` is now, every link on the way to it resolved: on Linux, the
/// target of its link in `/proc/self/fd`, which follows renames of its directories, and on
/// Windows its final path, which names its volume by a drive letter or a mount point, or else by
/// the volume's GUID.
fn location_of(file: &File) -> io::Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;

        fs::read_link(Path::new("/proc/self/fd").join(file.as_raw_fd().to_string()))
    }
    #[cfg(windows)]
    {
        // A volume without a drive letter or a mount point has no path by a drive.
        crate::host::seal::final_path(file)
            .or_else(|error| crate::host::seal::final_volume_path(file).map_err(|_| error))
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = file;
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

/// Refuses the clone's scratch image at `path`, which `checked` holds open, unless it has a single
/// name: another name, a hard link, could make it a file of a snapshot that the clone's guest then
/// writes, of one of the store's generations or captures, or of another store, which the image's
/// location does not show. The request supplied the image, so its refusal is invalid settings,
/// while the failure to count its names is the host's.
fn check_single_name(checked: &CheckedScratch, path: &Path) -> Result<(), Fault> {
    let links = platform::link_count(checked.file()).map_err(|error| {
        Fault::host(
            Error::io(format!(
                "cannot count the names of the scratch image {}",
                path.display()
            ))
            .with_source(error),
        )
    })?;
    if links > 1 {
        return Err(Fault::new(
            Evidence::InvalidSettings,
            Error::unsupported(format!(
                "the scratch image {} has {links} names, but a clone's scratch image must have a \
                 single name: another, a hard link, could make it a file of a snapshot, which \
                 every clone of its generation restores in place",
                path.display()
            )),
        ));
    }
    Ok(())
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
struct StoreFiles(Vec<(PathBuf, (u64, u128))>);

impl StoreFiles {
    /// Returns the regular files that the store of the sandbox of `locked` owns now, as
    /// [`LockedStore::owned_entries`] lists its entries: its files, among them its log and the
    /// clone's activation record, which a launch replaces, and the clone binding, which a release
    /// removes, and its temporary files.
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

/// Refuses `found`, the geometry of a clone's image, which messages call `what`, unless it fits
/// `capacity`, the geometry of the template's placeholder, which a restore presents over the
/// image: of other block sizes, [`Evidence::SuppliedGeometry`], or longer,
/// [`Evidence::SuppliedLength`]. A shorter image fills the capacity's first blocks, and the
/// restore reads zeros past its end.
fn check_capacity(found: BlockGeometry, capacity: BlockGeometry, what: &str) -> Result<(), Fault> {
    check_blocks(found, capacity, what, "placeholder")?;
    if found.length > capacity.length {
        return Err(Fault::new(
            Evidence::SuppliedLength,
            Error::unsupported(format!(
                "{what} is {} bytes long, longer than the template's placeholder, whose {} bytes \
                 are the capacity of every clone's image",
                found.length, capacity.length
            )),
        ));
    }
    Ok(())
}

/// Refuses `found`, the geometry of the file of a clone's block device that the request supplied,
/// which messages call `what`, unless it is `expected`, the geometry that the template's `device`
/// presented: of another length, [`Evidence::SuppliedLength`], or of other block sizes,
/// [`Evidence::SuppliedGeometry`].
fn check_geometry(
    found: BlockGeometry,
    expected: BlockGeometry,
    what: &str,
    device: &str,
) -> Result<(), Fault> {
    if found.length != expected.length {
        return Err(Fault::new(
            Evidence::SuppliedLength,
            Error::unsupported(format!(
                "{what} is {} bytes long, not the {} bytes of the template's {device}",
                found.length, expected.length
            )),
        ));
    }
    check_blocks(found, expected, what, device)
}

/// Refuses `found`, the geometry of the file of a clone's block device that the request supplied,
/// which messages call `what`, unless it presents the logical and physical block sizes of
/// `expected`, the geometry that the template's `device` presented, [`Evidence::SuppliedGeometry`].
fn check_blocks(
    found: BlockGeometry,
    expected: BlockGeometry,
    what: &str,
    device: &str,
) -> Result<(), Fault> {
    if found.logical_block_size != expected.logical_block_size
        || found.physical_block_size != expected.physical_block_size
    {
        return Err(Fault::new(
            Evidence::SuppliedGeometry,
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
