//! The application of the record of the Activate that a clone's [`Activation`] accepted:
//! [`apply`](fn@apply), which activates the clone's root and workload with the record's settings,
//! as at a cold boot, as the [module](super::super#activation) describes.

#[cfg(test)]
mod tests;

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(doc)]
use super::Activation;
use crate::activation::{self, ActivationRecord, Configuration, WorkloadIdentity};
use crate::gpt::Expected;
#[cfg(doc)]
use crate::guest::entry::AccountMode;
use crate::guest::entry::{self, Entry, Identity};
use crate::guest::root::{self, Assembly, AssemblyError, Device, Root, TeardownError};

/// The inputs of a clone's root assembly that its guest takes from its own configuration, which
/// its template captured, so that every clone of the template has the same: the runtime
/// directory, the block devices, which their MMIO windows name, with the sizes that they must
/// have, and how long the assembly waits for each. [`apply`](fn@apply) takes the rest from the
/// activation record.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RootTemplate {
    /// The runtime directory, below which the root is mounted, as [`Assembly::runtime`] tells.
    pub runtime: PathBuf,
    /// The `distro` block device, the template's placeholder, which holds each clone's image.
    pub distro: Device,
    /// The scratch block device, which holds the upper of a record whose upper is the scratch,
    /// and which a record whose upper is a tmpfs leaves alone.
    pub scratch: Device,
    /// How long the assembly waits for each device to appear.
    pub timeout: Duration,
}

/// A clone's root and workload, which [`apply`](fn@apply) activated: the [`Root`] that it
/// assembled, which owns the root's mounts, and the identity in which every execution runs, if
/// the record has a workload identity.
///
/// Without one, the adapter gives each execution its own identity, which [`Entry::check`] checks
/// when the adapter prepares the execution's entry, as at a cold boot. Dropping it tears the root
/// down, as dropping a [`Root`] does.
#[derive(Debug)]
pub struct Applied {
    /// The assembled root.
    root: Root,
    /// The identity in which every execution runs, if the record has a workload identity.
    identity: Option<Identity>,
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

    /// Returns the assembled root, and the identity in which every execution runs, if any.
    pub fn into_parts(self) -> (Root, Option<Identity>) {
        (self.root, self.identity)
    }
}

/// Applies `record`, the record of the Activate that a clone's [`Activation`] accepted, to the
/// clone, as at a cold boot, and returns the root and the workload that it activated, which the
/// adapter keeps once it reported the result to [`Activation::complete`].
///
/// In order, it:
///
/// 1. takes the inputs of the root's [`Assembly`] from the record: its image, an EROFS image,
///    with the UUID that the record expects of it, if any, or a GPT disk, with the disk GUID that
///    the record expects of it, if any, and the bound of its first partition; its upper, a tmpfs,
///    or the scratch, with the record's journal policy; and its overlay mode; and, from
///    `template`, the runtime directory, the devices, and the timeout;
/// 2. for a GPT disk, asks the kernel to reread the disk's partition table, once the distro
///    device appeared, so that the block devices of the disk's partitions are those of the
///    clone's disk, rather than those of the template's placeholder, which the kernel created
///    when it found the placeholder: the kernel then has parsed the same read-only bytes that
///    the assembly reads and checks, as at a cold boot. A failure, before anything is mounted,
///    fails with [`ApplyError::Gpt`];
/// 3. assembles the root as [`root::assemble`] does, which checks the devices, and the image as
///    the record expects it, and mounts the root, but with a fresh upper: it refuses the upper's
///    `upper` or `work` directory if it exists and is not empty, with
///    [`root::Error::NotFresh`], since a clone's root starts as its image alone, while a cold
///    sandbox's scratch keeps its upper across boots. That check reads at most one entry of
///    each directory, and no file. A failure fails with [`ApplyError::Assembly`], once the
///    assembly unmounted what it mounted;
/// 4. if the record has a workload identity, checks its account in the assembled root, before
///    Ready, as NVX's init checks it at boot: the identity is [`Identity::new`] of the record's
///    user and group IDs, without supplementary groups or capabilities, in
///    [existing-account](AccountMode::Existing) mode, whose account [`Entry::prepare`] reads and
///    checks below the root. A refusal fails with [`ApplyError::Account`], once the application
///    tore the root down.
///
/// It applies none of the record's other settings: its export, its network, its hostname, and
/// its machine ID. It starts no process and generates no ID, and it never powers the VM off: a
/// failure, which the adapter reports to [`Activation::complete`], fails the activation finally,
/// and the adapter ends the VM.
///
/// It needs the guest's root user, in the guest's initial mount namespace, as
/// [`root::assemble`] does.
pub fn apply(record: &ActivationRecord, template: &RootTemplate) -> Result<Applied, ApplyError> {
    let (root, identity) = apply_with(&mut LiveSystem, record, template)?;
    Ok(Applied { root, identity })
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
}

impl fmt::Display for ApplyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Gpt(error) => write!(
                formatter,
                "the reread of the GPT disk's partition table failed: {error}"
            ),
            Self::Assembly(error) => write!(formatter, "the root's assembly failed: {error}"),
            Self::Account { error, teardown } => {
                write!(formatter, "the root refuses the workload identity: {error}")?;
                match teardown {
                    Some(teardown) => write!(formatter, "; then {teardown}"),
                    None => Ok(()),
                }
            }
        }
    }
}

impl std::error::Error for ApplyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Gpt(error) => Some(error),
            Self::Assembly(error) => Some(error),
            Self::Account { error, .. } => Some(error),
        }
    }
}

/// The calls through which an application reaches the system: the guest kernel's, in a VM, and a
/// fake's, which records them, in tests.
trait System {
    /// What an assembly mounted: a [`Root`], in a VM.
    type Root;

    /// Asks the kernel to reread the partition table of the GPT disk on the distro device
    /// `distro`, once it appeared, waiting for it for as long as `timeout` allows, as
    /// [`root::reread_partitions`] does.
    fn reread(&mut self, distro: Device, timeout: Duration) -> Result<(), root::Error>;

    /// Assembles the root of `assembly` with a fresh upper, as [`root::assemble_fresh`] does.
    fn assemble(&mut self, assembly: &Assembly) -> Result<Self::Root, AssemblyError>;

    /// Reads and checks the account of `identity` in `root`, as [`check_account`] does.
    fn account(&mut self, root: &Self::Root, identity: &Identity) -> Result<(), entry::Error>;

    /// Tears `root` down, as [`Root::teardown`] does.
    fn teardown(&mut self, root: Self::Root) -> Result<(), TeardownError>;
}

/// The guest kernel's calls.
struct LiveSystem;

impl System for LiveSystem {
    type Root = Root;

    fn reread(&mut self, distro: Device, timeout: Duration) -> Result<(), root::Error> {
        root::reread_partitions(distro, timeout)
    }

    fn assemble(&mut self, assembly: &Assembly) -> Result<Root, AssemblyError> {
        root::assemble_fresh(assembly)
    }

    fn account(&mut self, root: &Root, identity: &Identity) -> Result<(), entry::Error> {
        check_account(root.path(), identity)
    }

    fn teardown(&mut self, root: Root) -> Result<(), TeardownError> {
        root.teardown()
    }
}

/// Applies `record` with `template` through `system`, as [`apply`](fn@apply) does, and returns
/// the root that it assembled, and the identity in which every execution runs, if any.
fn apply_with<S: System>(
    system: &mut S,
    record: &ActivationRecord,
    template: &RootTemplate,
) -> Result<(S::Root, Option<Identity>), ApplyError> {
    let configuration = record.configuration();
    let assembly = assembly(template, configuration);
    let identity = configuration.workload.identity.map(workload_identity);
    if let root::Image::Gpt { .. } = assembly.image {
        system
            .reread(assembly.distro, assembly.timeout)
            .map_err(ApplyError::Gpt)?;
    }
    let root = system.assemble(&assembly).map_err(ApplyError::Assembly)?;
    if let Some(identity) = &identity
        && let Err(error) = system.account(&root, identity)
    {
        let teardown = system.teardown(root).err();
        return Err(ApplyError::Account { error, teardown });
    }
    Ok((root, identity))
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
