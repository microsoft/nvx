//! The application of the record of the Activate that a clone's [`Activation`] accepted:
//! [`apply`](fn@apply), which activates the clone's root and workload with the record's settings,
//! as at a cold boot, as the [module](super::super#activation) describes.

#[cfg(test)]
mod tests;

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[cfg(doc)]
use super::Activation;
use crate::activation::{self, ActivationRecord, Configuration, MachineIdMode, WorkloadIdentity};
use crate::gpt::Expected;
#[cfg(doc)]
use crate::guest::entry::AccountMode;
use crate::guest::entry::{self, Entry, Identity, MachineId, RuntimeBinds};
use crate::guest::mappings;
use crate::guest::root::{self, Assembly, AssemblyError, Device, Root, TeardownError};

/// The name of the file, in the template's [runtime directory](RootTemplate::runtime), to which
/// [`apply`](fn@apply) writes the machine ID that it generates for a record that asks for one, as
/// NVX's managed agent writes its own to `/run/nvx/workload-machine-id`.
pub const MACHINE_ID_FILE: &str = "workload-machine-id";

/// The inputs of a clone's root assembly that its guest takes from its own configuration, which
/// its template captured, so that every clone of the template has the same: the runtime
/// directory, the block devices, which their MMIO windows name, with the sizes that they must
/// have, and how long the assembly waits for each. [`apply`](fn@apply) takes the rest from the
/// activation record.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RootTemplate {
    /// The runtime directory, below which the root is mounted, as [`Assembly::runtime`] tells,
    /// and in which the application writes a machine ID that it generates, as
    /// [`MACHINE_ID_FILE`].
    pub runtime: PathBuf,
    /// The `distro` block device, the template's placeholder, which holds each clone's image.
    pub distro: Device,
    /// The scratch block device, which holds the upper of a record whose upper is the scratch,
    /// and which a record whose upper is a tmpfs leaves alone.
    pub scratch: Device,
    /// How long the assembly waits for each device to appear, until the deadline that
    /// [`apply`](fn@apply) takes at the latest.
    pub timeout: Duration,
}

/// A clone's root and workload, which [`apply`](fn@apply) activated: the [`Root`] that it
/// assembled, which owns the root's mounts, the runtime's binds that it made on the root, in the
/// caller's mount namespace, the identity in which every execution runs, if the record has a
/// workload identity, and the workload's hostname and machine ID, the record's.
///
/// Without a workload identity, the adapter gives each execution its own identity, which
/// [`Entry::check`] checks when the adapter prepares the execution's entry, as at a cold boot.
/// Every execution's entry takes the [hostname](Self::hostname), through [`Entry::hostname`], and
/// sees the [machine ID](Self::machine_id) through the runtime's binds, which each execution's
/// mount namespace copies from the caller's. [`teardown`](Self::teardown) removes the binds, then
/// tears the root down, and dropping it does the same, ignoring failures, as dropping a [`Root`]
/// does.
#[derive(Debug)]
pub struct Applied {
    /// The runtime's binds on the root, which a drop removes before the root drops: the binds
    /// keep the root's mount busy.
    binds: Binds,
    /// The assembled root.
    root: Root,
    /// The identity in which every execution runs, if the record has a workload identity.
    identity: Option<Identity>,
    /// The hostname of every execution: the record's.
    hostname: Vec<u8>,
    /// The machine ID that every execution sees at `/etc/machine-id`.
    machine_id: MachineId,
}

impl Applied {
    /// Returns the assembled root.
    pub fn root(&self) -> &Root {
        &self.root
    }

    /// Returns the assembled root, to mount live shares and mappings in it.
    pub fn root_mut(&mut self) -> &mut Root {
        &mut self.root
    }

    /// Returns the identity in which every execution runs: [`Identity::new`] of the record's
    /// workload identity, without supplementary groups or capabilities, in existing-account mode,
    /// whose account the root holds. Returns `None` if the record has no workload identity, so
    /// that each execution names its own.
    pub fn identity(&self) -> Option<&Identity> {
        self.identity.as_ref()
    }

    /// Returns the hostname of every execution, the record's, which the adapter gives each
    /// execution's entry through [`Entry::hostname`], and which each execution then sets in a UTS
    /// namespace of its own.
    pub fn hostname(&self) -> &[u8] {
        &self.hostname
    }

    /// Returns the machine ID that every execution sees at `/etc/machine-id`: the image's own,
    /// [`MachineId::Image`], or [`MachineId::Generated`], with the file of [`MACHINE_ID_FILE`]
    /// that the application generated once, and bound read-only over the root's
    /// `/etc/machine-id`, for every execution.
    pub fn machine_id(&self) -> &MachineId {
        &self.machine_id
    }

    /// Removes the runtime's binds, then tears the root down, as [`Root::teardown`] does, in the
    /// caller's mount namespace, which must be the one where the application made them: the binds
    /// keep the root's mount busy, and a teardown detaches no busy mount.
    ///
    /// It attempts both steps, whatever failed before, and fails with the binds that stay
    /// mounted, and the unmounts of the root's teardown that failed, whose mounts stay.
    pub fn teardown(self) -> Result<(), AppliedTeardownError> {
        let Self {
            mut binds, root, ..
        } = self;
        teardown_with(&mut LiveSystem, binds.take(), root)
    }

    /// Returns the assembled root, the runtime's binds on it, and the identity in which every
    /// execution runs, if any, for an adapter that keeps them apart: it then removes the binds
    /// with [`RuntimeBinds::remove`] before it tears the root down, in the same mount namespace,
    /// since dropping them leaves them mounted.
    pub fn into_parts(self) -> (Root, RuntimeBinds, Option<Identity>) {
        let Self {
            mut binds,
            root,
            identity,
            ..
        } = self;
        (root, binds.take(), identity)
    }
}

/// The runtime's binds on a clone's root, which a drop removes, ignoring failures, before the root
/// that they keep busy drops.
#[derive(Debug)]
struct Binds(Option<RuntimeBinds>);

impl Binds {
    /// Takes the binds, which the drop then leaves alone.
    fn take(&mut self) -> RuntimeBinds {
        self.0
            .take()
            .expect("an application's binds are taken at most once")
    }
}

impl Drop for Binds {
    fn drop(&mut self) {
        if let Some(binds) = self.0.take() {
            let _ = binds.remove();
        }
    }
}

/// Why [`Applied::teardown`] failed: the runtime's binds that it failed to remove, which stay
/// mounted and keep the root's mount busy, and the unmounts of the root's teardown that failed,
/// whose mounts stay. At least one of them failed.
#[derive(Debug)]
pub struct AppliedTeardownError {
    /// The binds that stay mounted, if any.
    binds: Option<mappings::TeardownError>,
    /// The unmounts of the root's teardown that failed, if any.
    root: Option<TeardownError>,
}

impl AppliedTeardownError {
    /// Returns the runtime's binds that the teardown failed to remove, if any, which stay mounted.
    pub fn binds(&self) -> Option<&mappings::TeardownError> {
        self.binds.as_ref()
    }

    /// Returns the unmounts of the root's teardown that failed, if any, whose mounts stay.
    pub fn root(&self) -> Option<&TeardownError> {
        self.root.as_ref()
    }
}

impl fmt::Display for AppliedTeardownError {
    /// Writes the binds' failures, then the root's, separated by `; `.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.binds, &self.root) {
            (Some(binds), Some(root)) => write!(formatter, "{binds}; {root}"),
            (Some(binds), None) => binds.fmt(formatter),
            (None, Some(root)) => root.fmt(formatter),
            (None, None) => Ok(()),
        }
    }
}

impl std::error::Error for AppliedTeardownError {}

/// Applies `record`, the record of the Activate that a clone's [`Activation`] accepted, to the
/// clone, as at a cold boot, by `deadline`, the activation's [deadline](Activation::deadline), if
/// any, and returns the root and the workload that it activated, which the adapter keeps once it
/// reported the result to [`Activation::complete`].
///
/// In order, it:
///
/// 1. takes the inputs of the root's [`Assembly`] from the record: its image, an EROFS image,
///    with the UUID that the record expects of it, if any, or a GPT disk, with the disk GUID that
///    the record expects of it, if any, and the bound of its first partition; its upper, a tmpfs,
///    or the scratch, with the record's journal policy; and its overlay mode; and, from
///    `template`, the runtime directory, the devices, and the timeout. It takes the workload's
///    hostname and machine-ID mode from the record too;
/// 2. for a GPT disk, asks the kernel to reread the disk's partition table, once the distro
///    device appeared, so that the block devices of the disk's partitions are those of the
///    clone's disk, rather than those of the template's placeholder, which the kernel created
///    when it found the placeholder: the kernel then has parsed the same read-only bytes that
///    the assembly reads and checks, as at a cold boot. It waits for the distro device as the
///    assembly does, never past `deadline`. A failure, before anything is mounted, fails with
///    [`ApplyError::Gpt`];
/// 3. assembles the root as [`root::assemble`] does, which checks the devices, and the image as
///    the record expects it, and mounts the root, but with a fresh upper: it refuses the upper's
///    `upper` or `work` directory if it exists and is not empty, with
///    [`root::Error::NotFresh`], since a clone's root starts as its image alone, while a cold
///    sandbox's scratch keeps its upper across boots. That check reads at most one entry of
///    each directory, and no file. A failure fails with [`ApplyError::Assembly`], once the
///    assembly unmounted what it mounted. It waits for each device as long as the template's
///    timeout allows, but never past `deadline`: a device that has not appeared by then fails
///    the assembly, as at its timeout, so that the application ends by the activation's
///    deadline, past which [`Activation::complete`] fails the activation as expired, whatever
///    the result;
/// 4. if the record has a workload identity, checks its account in the assembled root, before
///    Ready, as NVX's init checks it at boot: the identity is [`Identity::new`] of the record's
///    user and group IDs, without supplementary groups or capabilities, in
///    [existing-account](AccountMode::Existing) mode, whose account [`Entry::prepare`] reads and
///    checks below the root. A refusal fails with [`ApplyError::Account`], once the application
///    tore the root down;
/// 5. if the record asks for a generated machine ID, generates it, once, with
///    [`MachineId::generate`], from the kernel's random number generator, which the clone's
///    repair reseeded before the activation accepted the record, so that no two clones of a
///    template share it, to [`MACHINE_ID_FILE`] in the template's runtime directory. A file there
///    already, as once a boot generated its ID, or one that cannot be created or written, fails
///    with [`ApplyError::MachineId`], once the application tore the root down;
/// 6. binds the runtime's files into the root, for every execution, with
///    [`Entry::bind_runtime`], in the caller's mount namespace, of which each execution's starts
///    as a copy: a generated machine ID's file, read-only over the root's `/etc/machine-id`, and,
///    with the image's own machine ID, nothing. The entry checks the record's hostname first,
///    which every execution's entry then takes, through [`Entry::hostname`]. A failure fails with
///    [`ApplyError::View`], once the binds that it made were removed, and the application tore
///    the root down.
///
/// The activation answers a retry of the Activate without applying the record again, so every
/// execution, and every retry, sees the same machine ID. A failure after the generation leaves
/// the machine ID's file, since the activation fails finally, and the adapter ends the VM.
///
/// It applies none of the record's other settings: its export, and its network. It starts no
/// process, generates at most one ID, after the activation accepted the record, and never powers
/// the VM off: a failure, which the adapter reports to [`Activation::complete`], fails the
/// activation finally, and the adapter ends the VM.
///
/// It needs the guest's root user, in the guest's initial mount namespace, as
/// [`root::assemble`] does, and the bind of a generated machine ID needs Linux 6.8 or later, as
/// [`Entry::bind_runtime`] does.
pub fn apply(
    record: &ActivationRecord,
    template: &RootTemplate,
    deadline: Option<Instant>,
) -> Result<Applied, ApplyError> {
    let activated = apply_with(&mut LiveSystem, record, template, deadline)?;
    Ok(Applied {
        binds: Binds(Some(activated.binds)),
        root: activated.root,
        identity: activated.identity,
        hostname: activated.hostname,
        machine_id: activated.machine_id,
    })
}

/// Why [`apply`](fn@apply) failed, which the adapter reports to [`Activation::complete`].
///
/// Nothing that the application mounted stays mounted, unless the error holds the unmounts that
/// failed, whose mounts stay.
#[derive(Debug)]
#[non_exhaustive]
pub enum ApplyError {
    /// The partition table of the record's GPT disk could not be reread, before anything was
    /// mounted, as this error tells: the distro device did not appear in time, or the kernel
    /// refused the reread, as it does while one of the disk's partitions is open. The block
    /// devices of the disk's partitions need not be the clone's then: the kernel created them
    /// when it found the template's placeholder.
    Gpt(root::Error),
    /// The root's assembly failed, as this error tells, and unmounted what it had mounted, unless
    /// the error holds the unmounts that failed. It fails with [`root::Error::NotFresh`] if the
    /// upper's `upper` or `work` directory exists and is not empty.
    Assembly(AssemblyError),
    /// Existing-account mode refused the record's workload identity in the assembled root, which
    /// the application then tore down.
    Account {
        /// Why the identity was refused: the root holds no account of its user ID whose primary
        /// group is its group ID, and which its group database holds, or its account databases
        /// cannot be read.
        error: entry::Error,
        /// The unmounts of the root's teardown that failed, if any, whose mounts stay.
        teardown: Option<TeardownError>,
    },
    /// The machine ID that the record asks the guest to generate could not be written to
    /// [`MACHINE_ID_FILE`] in the template's runtime directory, as this error tells: a file is
    /// there already, as once a boot generated its ID, or the file cannot be created or written.
    /// The application then tore the root down.
    MachineId {
        /// Why the machine ID could not be written.
        error: io::Error,
        /// The unmounts of the root's teardown that failed, if any, whose mounts stay.
        teardown: Option<TeardownError>,
    },
    /// The workload's view could not be set in the assembled root, as this error tells: the
    /// entry refused the record's hostname, or could not bind the generated machine ID's file
    /// over the root's `/etc/machine-id`, after which it removed the binds that it made, unless
    /// the error names those that stay. The application then tore the root down.
    View {
        /// Why the view could not be set, as [`Entry::bind_runtime`] tells.
        error: entry::Error,
        /// The unmounts of the root's teardown that failed, if any, whose mounts stay.
        teardown: Option<TeardownError>,
    },
}

impl fmt::Display for ApplyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let teardown = match self {
            Self::Gpt(error) => {
                return write!(
                    formatter,
                    "the reread of the GPT disk's partition table failed: {error}"
                );
            }
            Self::Assembly(error) => {
                return write!(formatter, "the root's assembly failed: {error}");
            }
            Self::Account { error, teardown } => {
                write!(formatter, "the root refuses the workload identity: {error}")?;
                teardown
            }
            Self::MachineId { error, teardown } => {
                write!(
                    formatter,
                    "the workload's machine ID cannot be generated: {error}"
                )?;
                teardown
            }
            Self::View { error, teardown } => {
                write!(
                    formatter,
                    "the workload's hostname or machine ID cannot be set: {error}"
                )?;
                teardown
            }
        };
        match teardown {
            Some(teardown) => write!(formatter, "; then {teardown}"),
            None => Ok(()),
        }
    }
}

impl std::error::Error for ApplyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Gpt(error) => Some(error),
            Self::Assembly(error) => Some(error),
            Self::Account { error, .. } | Self::View { error, .. } => Some(error),
            Self::MachineId { error, .. } => Some(error),
        }
    }
}

/// The calls through which an application reaches the system: the guest kernel's, in a VM, and a
/// fake's, which records them, in tests.
trait System {
    /// What an assembly mounted: a [`Root`], in a VM.
    type Root;

    /// The runtime's binds that a bind made: [`RuntimeBinds`], in a VM.
    type Binds;

    /// Asks the kernel to reread the partition table of the GPT disk on the distro device
    /// `distro`, once it appeared, waiting for it for as long as `timeout` allows, but until
    /// `deadline` at the latest, if any, as [`root::reread_partitions`] does.
    fn reread(
        &mut self,
        distro: Device,
        timeout: Duration,
        deadline: Option<Instant>,
    ) -> Result<(), root::Error>;

    /// Assembles the root of `assembly` with a fresh upper, as [`root::assemble_fresh`] does,
    /// waiting for each device until `deadline` at the latest, if any.
    fn assemble(
        &mut self,
        assembly: &Assembly,
        deadline: Option<Instant>,
    ) -> Result<Self::Root, AssemblyError>;

    /// Reads and checks the account of `identity` in `root`, as [`check_account`] does.
    fn account(&mut self, root: &Self::Root, identity: &Identity) -> Result<(), entry::Error>;

    /// Generates a machine ID, and writes it to a new file at `path`, as [`MachineId::generate`]
    /// does.
    fn generate(&mut self, path: &Path) -> io::Result<MachineId>;

    /// Binds the runtime's files of `view` into `root`, for every execution, in the caller's
    /// mount namespace, as [`Entry::bind_runtime`] does with the view's [entry](View::entry).
    fn bind(&mut self, root: &Self::Root, view: &View) -> Result<Self::Binds, entry::Error>;

    /// Removes `binds`, as [`RuntimeBinds::remove`] does.
    fn remove(&mut self, binds: Self::Binds) -> Result<(), mappings::TeardownError>;

    /// Tears `root` down, as [`Root::teardown`] does.
    fn teardown(&mut self, root: Self::Root) -> Result<(), TeardownError>;
}

/// The guest kernel's calls.
struct LiveSystem;

impl System for LiveSystem {
    type Root = Root;
    type Binds = RuntimeBinds;

    fn reread(
        &mut self,
        distro: Device,
        timeout: Duration,
        deadline: Option<Instant>,
    ) -> Result<(), root::Error> {
        root::reread_partitions(distro, timeout, deadline)
    }

    fn assemble(
        &mut self,
        assembly: &Assembly,
        deadline: Option<Instant>,
    ) -> Result<Root, AssemblyError> {
        root::assemble_fresh(assembly, deadline)
    }

    fn account(&mut self, root: &Root, identity: &Identity) -> Result<(), entry::Error> {
        check_account(root.path(), identity)
    }

    fn generate(&mut self, path: &Path) -> io::Result<MachineId> {
        MachineId::generate(path)
    }

    fn bind(&mut self, root: &Root, view: &View) -> Result<RuntimeBinds, entry::Error> {
        view.entry(root.path()).bind_runtime()
    }

    fn remove(&mut self, binds: RuntimeBinds) -> Result<(), mappings::TeardownError> {
        binds.remove()
    }

    fn teardown(&mut self, root: Root) -> Result<(), TeardownError> {
        root.teardown()
    }
}

/// The workload's view of its root, which the application sets for every execution: the
/// identity of the entry through which it binds the runtime's files, the hostname, and the
/// machine ID.
#[derive(Clone, Debug, PartialEq, Eq)]
struct View {
    /// The identity of the entry that binds the runtime's files: the record's workload identity,
    /// or, without one, the user and group ID 0, since the entry binds the same files whatever
    /// its identity.
    identity: Identity,
    /// The hostname of every execution: the record's.
    hostname: Vec<u8>,
    /// The machine ID that every execution sees.
    machine_id: MachineId,
}

impl View {
    /// Returns the entry into the root at `root` that binds the view's runtime files, and checks
    /// its hostname, with [`Entry::bind_runtime`].
    fn entry(&self, root: &Path) -> Entry {
        Entry::new(root, self.identity.clone())
            .hostname(self.hostname.clone())
            .machine_id(self.machine_id.clone())
    }
}

/// What an application activated through a system, which an [`Applied`] holds in a VM: the
/// root, the runtime's binds on it, the identity in which every execution runs, if any, and the
/// workload's hostname and machine ID.
#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
struct Activated<R, B> {
    /// The assembled root.
    root: R,
    /// The runtime's binds on the root.
    binds: B,
    /// The identity in which every execution runs, if the record has a workload identity.
    identity: Option<Identity>,
    /// The hostname of every execution.
    hostname: Vec<u8>,
    /// The machine ID that every execution sees.
    machine_id: MachineId,
}

/// Applies `record` with `template` by `deadline`, if any, through `system`, as
/// [`apply`](fn@apply) does, and returns what it activated.
fn apply_with<S: System>(
    system: &mut S,
    record: &ActivationRecord,
    template: &RootTemplate,
    deadline: Option<Instant>,
) -> Result<Activated<S::Root, S::Binds>, ApplyError> {
    let configuration = record.configuration();
    let assembly = assembly(template, configuration);
    let workload = &configuration.workload;
    let identity = workload.identity.map(workload_identity);
    if let root::Image::Gpt { .. } = assembly.image {
        system
            .reread(assembly.distro, assembly.timeout, deadline)
            .map_err(ApplyError::Gpt)?;
    }
    let root = system
        .assemble(&assembly, deadline)
        .map_err(ApplyError::Assembly)?;
    if let Some(identity) = &identity
        && let Err(error) = system.account(&root, identity)
    {
        let teardown = system.teardown(root).err();
        return Err(ApplyError::Account { error, teardown });
    }
    let machine_id = match workload.machine_id {
        MachineIdMode::Image => MachineId::Image,
        MachineIdMode::Generated => {
            match system.generate(&template.runtime.join(MACHINE_ID_FILE)) {
                Ok(machine_id) => machine_id,
                Err(error) => {
                    let teardown = system.teardown(root).err();
                    return Err(ApplyError::MachineId { error, teardown });
                }
            }
        }
    };
    let view = View {
        identity: identity.clone().unwrap_or_else(|| Identity::new(0, 0)),
        hostname: workload.hostname.clone(),
        machine_id,
    };
    let binds = match system.bind(&root, &view) {
        Ok(binds) => binds,
        Err(error) => {
            let teardown = system.teardown(root).err();
            return Err(ApplyError::View { error, teardown });
        }
    };
    Ok(Activated {
        root,
        binds,
        identity,
        hostname: view.hostname,
        machine_id: view.machine_id,
    })
}

/// Removes `binds`, then tears `root` down, through `system`, as [`Applied::teardown`] does,
/// attempting both whatever failed before.
fn teardown_with<S: System>(
    system: &mut S,
    binds: S::Binds,
    root: S::Root,
) -> Result<(), AppliedTeardownError> {
    let binds = system.remove(binds).err();
    let root = system.teardown(root).err();
    if binds.is_none() && root.is_none() {
        return Ok(());
    }
    Err(AppliedTeardownError { binds, root })
}

/// Returns the assembly of the root that `configuration` sets, with `template`'s runtime
/// directory, devices, and timeout.
fn assembly(template: &RootTemplate, configuration: &Configuration) -> Assembly {
    let image = match configuration.image {
        activation::Image::Erofs { uuid, .. } => root::Image::Erofs { uuid },
        activation::Image::Gpt {
            disk_guid,
            first_partition_bound,
            ..
        } => root::Image::Gpt {
            expected: match disk_guid {
                Some(guid) => Expected::new().disk_guid(guid),
                None => Expected::new(),
            },
            // A larger bound than the guest can address bounds nothing more.
            first_partition_bound: usize::try_from(first_partition_bound).unwrap_or(usize::MAX),
        },
    };
    let upper = match configuration.upper {
        activation::Upper::Tmpfs => root::Upper::Tmpfs,
        activation::Upper::Scratch { journal, .. } => root::Upper::Scratch {
            device: template.scratch,
            journal: match journal {
                activation::Journal::Allowed => root::Journal::Allowed,
                activation::Journal::Forbidden => root::Journal::Forbidden,
            },
        },
    };
    let overlay = match configuration.overlay {
        activation::OverlayMode::TrustedLowers => root::OverlayMode::TrustedLowers,
        activation::OverlayMode::UntrustedLowers => root::OverlayMode::UntrustedLowers,
    };
    Assembly {
        runtime: template.runtime.clone(),
        distro: template.distro,
        image,
        upper,
        overlay,
        timeout: template.timeout,
    }
}

/// Returns the identity in which every execution of a workload with `identity` runs: its user
/// and group IDs, without supplementary groups or capabilities, whose account an entry checks in
/// existing-account mode.
fn workload_identity(identity: WorkloadIdentity) -> Identity {
    Identity::new(identity.uid, identity.gid)
}

/// Reads and checks the account of `identity` below the root at `root`, as an entry does in
/// existing-account mode before each execution: through [`Entry::prepare`].
fn check_account(root: &Path, identity: &Identity) -> Result<(), entry::Error> {
    Entry::new(root, identity.clone()).prepare().map(drop)
}
