//! The application of the record of the Activate that a clone's [`Activation`] accepted:
//! [`apply`](fn@apply), which activates the clone's root and workload with the record's settings,
//! as at a cold boot, as the [module](super::super#activation) describes.

#[cfg(test)]
mod tests;

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[cfg(doc)]
use super::Activation;
use crate::activation::{self, ActivationRecord, Configuration, WorkloadIdentity};
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
    /// How long the assembly waits for each device to appear, until the deadline that
    /// [`apply`](fn@apply) takes at the latest.
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
/// clone, as at a cold boot, by `deadline`, the activation's [deadline](Activation::deadline), if
/// any, and returns the root and the workload that it activated, which the adapter keeps once it
/// reported the result to [`Activation::complete`].
///
/// In order, it:
///
/// 1. takes the inputs of the root's [`Assembly`] from the record: its image, an EROFS image,
///    with the UUID that the record expects of it, if any; its upper, a tmpfs, or the scratch,
///    with the record's journal policy; and its overlay mode; and, from `template`, the runtime
///    directory, the devices, and the timeout. It refuses a GPT disk, with
///    [`ApplyError::Gpt`], before anything has a side effect;
/// 2. assembles the root as [`root::assemble`] does, which checks the devices, and the image as
///    the record expects it, and mounts the root, or fails with [`ApplyError::Assembly`], once it
///    unmounted what it mounted. It waits for each device as long as the template's timeout
///    allows, but never past `deadline`: a device that has not appeared by then fails the
///    assembly, as at its timeout, so that the application ends by the activation's deadline,
///    past which [`Activation::complete`] fails the activation as expired, whatever the result;
/// 3. if the record has a workload identity, checks its account in the assembled root, before
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
pub fn apply(
    record: &ActivationRecord,
    template: &RootTemplate,
    deadline: Option<Instant>,
) -> Result<Applied, ApplyError> {
    let (root, identity) = apply_with(&mut LiveSystem, record, template, deadline)?;
    Ok(Applied { root, identity })
}

/// Why [`apply`](fn@apply) failed, which the adapter reports to [`Activation::complete`].
///
/// Nothing that the application mounted stays mounted, unless the error holds the unmounts that
/// failed, whose mounts stay.
#[derive(Debug)]
#[non_exhaustive]
pub enum ApplyError {
    /// The record's image is a GPT disk, which the application refuses before anything has a
    /// side effect: the kernel created the block devices of a disk's partitions when it found the
    /// template's placeholder, so they need not be the clone's.
    Gpt,
    /// The root's assembly failed, as this error tells, and unmounted what it had mounted, unless
    /// the error holds the unmounts that failed.
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
            Self::Gpt => formatter
                .write_str("the record's image is a GPT disk, which a clone does not assemble"),
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
            Self::Gpt => None,
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

    /// Assembles the root of `assembly`, as [`root::assemble`] does, but waits for each device
    /// until `deadline` at the latest, if any.
    fn assemble(
        &mut self,
        assembly: &Assembly,
        deadline: Option<Instant>,
    ) -> Result<Self::Root, AssemblyError>;

    /// Reads and checks the account of `identity` in `root`, as [`check_account`] does.
    fn account(&mut self, root: &Self::Root, identity: &Identity) -> Result<(), entry::Error>;

    /// Tears `root` down, as [`Root::teardown`] does.
    fn teardown(&mut self, root: Self::Root) -> Result<(), TeardownError>;
}

/// The guest kernel's calls.
struct LiveSystem;

impl System for LiveSystem {
    type Root = Root;

    fn assemble(
        &mut self,
        assembly: &Assembly,
        deadline: Option<Instant>,
    ) -> Result<Root, AssemblyError> {
        root::assemble_until(assembly, deadline)
    }

    fn account(&mut self, root: &Root, identity: &Identity) -> Result<(), entry::Error> {
        check_account(root.path(), identity)
    }

    fn teardown(&mut self, root: Root) -> Result<(), TeardownError> {
        root.teardown()
    }
}

/// Applies `record` with `template` by `deadline`, if any, through `system`, as
/// [`apply`](fn@apply) does, and returns the root that it assembled, and the identity in which
/// every execution runs, if any.
fn apply_with<S: System>(
    system: &mut S,
    record: &ActivationRecord,
    template: &RootTemplate,
    deadline: Option<Instant>,
) -> Result<(S::Root, Option<Identity>), ApplyError> {
    let configuration = record.configuration();
    let assembly = assembly(template, configuration)?;
    let identity = configuration.workload.identity.map(workload_identity);
    let root = system
        .assemble(&assembly, deadline)
        .map_err(ApplyError::Assembly)?;
    if let Some(identity) = &identity
        && let Err(error) = system.account(&root, identity)
    {
        let teardown = system.teardown(root).err();
        return Err(ApplyError::Account { error, teardown });
    }
    Ok((root, identity))
}

/// Returns the assembly of the root that `configuration` sets, with `template`'s runtime
/// directory, devices, and timeout, or refuses a GPT disk.
fn assembly(
    template: &RootTemplate,
    configuration: &Configuration,
) -> Result<Assembly, ApplyError> {
    let image = match configuration.image {
        activation::Image::Erofs { uuid, .. } => root::Image::Erofs { uuid },
        activation::Image::Gpt { .. } => return Err(ApplyError::Gpt),
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
    Ok(Assembly {
        runtime: template.runtime.clone(),
        distro: template.distro,
        image,
        upper,
        overlay,
        timeout: template.timeout,
    })
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
