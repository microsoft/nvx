//! Application of an accepted activation record, without starting a process.
//!
//! This module is part of the guest's space, nvx-init's internal reference code, private to the
//! crate, which applies the record that the [activation](crate::guest::activation#activation)
//! accepted, as the adapter's own space. The root owns the VM-wide view's binds; the caller owns
//! ingress and the admission of its workloads.
//!
//! It activates the clone as at a cold boot. It assembles the root as a cold boot does, with the
//! record's image, an EROFS image whose UUID it checks if the record expects one, or a GPT disk
//! whose GUID it checks if the record expects one, its upper, a tmpfs or the scratch, with the
//! record's journal policy, and its overlay mode, and with the inputs that the guest takes from
//! its own configuration, which its template captured, a [`RootTemplate`]: the runtime directory,
//! the block devices, the host filesystem device's virtio-fs tag, the gateway of the template's
//! NIC, if it has one, and how long the assembly waits for each block device, but never past the
//! activation's deadline, so that the application ends by then. For a GPT disk, it first asks the
//! kernel to reread the disk's partition table, once the distro device appeared, by the same
//! deadline: the kernel created the block devices of partitions from the template's placeholder,
//! when it found it, and creates those of the clone's disk then, from the same read-only bytes
//! that the assembly reads and checks, as at a cold boot. The upper must be fresh: the assembly
//! refuses one whose `upper` or `work` directory exists and is not empty, since a clone's root
//! starts as its image alone, while a cold sandbox's scratch keeps its upper across boots; the
//! check reads at most one entry of each, and no file. The record holds no execution identity:
//! the adapter gives each execution the identity that the execution names, as for a sandbox
//! without a workload identity at a cold boot. If the record has an export, it mounts the host
//! export through the template's filesystem device, read-write, at `hostfs/root` in the runtime
//! directory, outside the root, where only the guest's root user reaches it: the record names no
//! mode, and OpenVMM limits each of the export's children to its own access. It applies the
//! record's VM-wide view, with the record's DNS server, if any. The root owns the machine-ID and
//! resolver binds, including any made before a later failure. The view sets the VM's hostname
//! and generates an ID only when the record requests one, after the repair's reseed. The
//! application starts no process. nvx-init keeps the root and the workload that it activated,
//! with the export's root, from which it maps the host's mapping table once the clone is ready,
//! the network's proxy endpoint, which it tells its workloads as at a cold boot, and the
//! publication and gateway with which it starts and supervises ingress. Their teardown closes the
//! export's root, then tears the root down with its view binds. An application that fails leaves
//! nothing that it mounted or bound mounted, unless its error holds the unmounts that failed.
//!
//! The application needs the guest's root user, in the guest's initial mount namespace, as its
//! assembly of a root does: for a GPT disk, it asks the kernel to reread the disk's partition
//! table, which needs `CAP_SYS_ADMIN`; it mounts the root, applies its VM-wide view, and, if it
//! fails, tears it down. It starts no process and generates at most one ID, a machine ID that the
//! record asks for. It mounts the host export of a record that has one, and writes the machine
//! ID's and the resolver's files in the runtime directory, whose binds over the root's
//! `/etc/machine-id` and `/etc/resolv.conf` need Linux 6.8 or later.

#[cfg(test)]
mod tests;

use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[cfg(doc)]
use super::Activation;
use crate::activation::{self, ActivationRecord, Configuration, Publication};
use crate::gpt::Expected;
use crate::guest::mappings::{Access, SourceRoot};
use crate::guest::root::{self, Assembly, AssemblyError, Device, Root, TeardownError};
#[cfg(doc)]
use crate::guest::snapshot::Devices;
use crate::guest::view::{self, MachineId};

pub use crate::guest::view::{MACHINE_ID_FILE, RESOLVER_FILE};

/// The root inputs captured by a template, shared by its clones.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RootTemplate {
    /// The runtime directory beneath which the root and its view files are assembled.
    pub(crate) runtime: PathBuf,
    /// The distro device, whose placeholder a clone replaces with its image.
    pub(crate) distro: Device,
    /// The scratch device, unused by a record with a RAM upper.
    pub(crate) scratch: Device,
    /// How long assembly waits for each device, never past the activation's deadline.
    pub(crate) timeout: Duration,
    /// The virtio-fs tag of the host filesystem device, as [`Devices::filesystem_tag`] names it.
    pub(crate) filesystem_tag: String,
    /// The template NIC's gateway, returned for the caller's ingress setup.
    pub(crate) gateway: Option<Ipv4Addr>,
}

/// The applied root and VM-wide view, with the settings the caller needs for ingress.
///
/// The application starts no process. The caller decides when to admit workloads and, for
/// published listeners, starts and supervises ingress itself. The root owns every view bind,
/// including one made before a later failure: its teardown removes them before its switch,
/// and nothing unmounts them afterwards.
#[derive(Debug)]
pub(crate) struct Applied {
    /// The export descriptor, dropped before the root unless the caller takes it.
    export: Option<SourceRoot>,
    /// The root, which also owns the view's binds.
    root: Root,
    /// The proxy endpoint, which the caller tells its workloads.
    proxy: Option<String>,
    /// Whether the record publishes listeners.
    publication: Publication,
    /// The template NIC's gateway, if any.
    gateway: Option<Ipv4Addr>,
}

impl Applied {
    /// Returns the assembled root.
    pub(crate) fn root(&self) -> &Root {
        &self.root
    }

    /// Returns the assembled root to mount runtime filesystems and shares in, and to switch into.
    pub(crate) fn root_mut(&mut self) -> &mut Root {
        &mut self.root
    }

    /// Returns the proxy endpoint for the caller to tell its workloads.
    pub(crate) fn proxy(&self) -> Option<&str> {
        self.proxy.as_deref()
    }

    /// Returns the record's publication, or unpublished when it has no network.
    pub(crate) fn publication(&self) -> Publication {
        self.publication
    }

    /// Returns the template NIC's gateway for the caller's ingress setup.
    pub(crate) fn gateway(&self) -> Option<Ipv4Addr> {
        self.gateway
    }

    /// Takes the export's root, whose descriptor must close before the root's teardown.
    pub(crate) fn take_export(&mut self) -> Option<SourceRoot> {
        self.export.take()
    }

    /// Closes the export descriptor, then tears the root down, including its view binds.
    ///
    /// As [`Root::teardown`] specifies, a switched root refuses teardown and unmounts nothing.
    pub(crate) fn teardown(self) -> Result<(), AppliedTeardownError> {
        teardown_with(&mut LiveSystem, self.export, self.root)
    }
}

/// Why an applied root's teardown failed: the root's teardown, which includes its view binds.
#[derive(Debug)]
pub(crate) struct AppliedTeardownError {
    /// The root's teardown failure, including its view binds.
    root: Option<TeardownError>,
}

impl fmt::Display for AppliedTeardownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.root {
            Some(root) => root.fmt(formatter),
            None => Ok(()),
        }
    }
}

impl std::error::Error for AppliedTeardownError {}

/// Applies the record accepted by a clone's [`Activation`], without starting any process.
///
/// Rereads a GPT image's partition table, assembles a fresh root, mounts the optional export,
/// and applies the record's VM-wide view through [`view::apply`], with the record's DNS server.
/// Device waits end by `deadline`, if supplied. The root owns all view binds, and application
/// failure closes the export and tears the root down, reporting any teardown failure.
///
/// The result reports the record's publication and the template's gateway. Published listeners
/// require a gateway, but the caller, not this application, starts and supervises ingress.
/// Each execution supplies its own identity; application reads no account database.
pub(crate) fn apply(
    record: &ActivationRecord,
    template: &RootTemplate,
    deadline: Option<Instant>,
) -> Result<Applied, ApplyError> {
    let activated = apply_with(&mut LiveSystem, record, template, deadline)?;
    Ok(Applied {
        export: activated.export,
        root: activated.root,
        proxy: activated.proxy,
        publication: activated.publication,
        gateway: activated.gateway,
    })
}

/// Why applying an activation record failed.
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum ApplyError {
    /// The distro device did not appear, or its GPT partition table could not be reread.
    Gpt(root::Error),
    /// Assembly failed, including a refusal of a non-fresh upper.
    Assembly(AssemblyError),
    /// The VM-wide view could not be prepared or its machine ID applied.
    View {
        /// The failure reported by [`view::apply`].
        error: crate::Error,
        /// Any root teardown failure, including view binds left mounted.
        teardown: Option<TeardownError>,
    },
    /// The host export could not be mounted.
    Export {
        /// The mount failure.
        error: root::Error,
        /// Any root teardown failure.
        teardown: Option<TeardownError>,
    },
    /// The record's network settings could not be applied.
    Network {
        /// Why the settings were refused.
        error: NetworkError,
        /// Any root teardown failure.
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
            Self::View { error, teardown } => {
                write!(formatter, "the workload's view cannot be set: {error}")?;
                teardown
            }
            Self::Export { error, teardown } => {
                write!(formatter, "the host export cannot be mounted: {error}")?;
                teardown
            }
            Self::Network { error, teardown } => {
                write!(formatter, "the network settings cannot be applied: {error}")?;
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
            Self::Gpt(error) | Self::Export { error, .. } => Some(error),
            Self::Assembly(error) => Some(error),
            Self::View { error, .. } => Some(error),
            Self::Network { error, .. } => Some(error),
        }
    }
}

/// Why an activation's network settings could not be applied.
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum NetworkError {
    /// The resolver file could not be written.
    Write(io::Error),
    /// The resolver bind retained by the root failed.
    Bind(crate::Error),
    /// The record publishes listeners but the template names no IPv4 gateway.
    Gateway,
}

impl fmt::Display for NetworkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Write(error) => write!(formatter, "the resolver file cannot be written: {error}"),
            Self::Bind(error) => error.fmt(formatter),
            Self::Gateway => formatter.write_str(
                "the network publishes the workload's listeners, but the template names no IPv4 \
                 gateway to announce them to",
            ),
        }
    }
}

impl std::error::Error for NetworkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Write(error) => Some(error),
            Self::Bind(error) => error.source(),
            Self::Gateway => None,
        }
    }
}

/// The assembly, export, and VM-wide view operations used by application.
trait System {
    /// The assembled root, which owns its view binds.
    type Root;
    /// The export descriptor.
    type Export;

    /// Rereads the distro device's partition table by the supplied deadline.
    fn reread(
        &mut self,
        distro: Device,
        timeout: Duration,
        deadline: Option<Instant>,
    ) -> Result<(), root::Error>;
    /// Assembles a fresh root, bounding device waits by the supplied deadline.
    fn assemble(
        &mut self,
        assembly: &Assembly,
        deadline: Option<Instant>,
    ) -> Result<Self::Root, AssemblyError>;
    /// Mounts an export and opens its root.
    fn mount_export(
        &mut self,
        root: &mut Self::Root,
        tag: &str,
        access: Access,
    ) -> Result<Self::Export, root::Error>;
    /// Closes the export descriptor before its root unmounts.
    fn close(&mut self, export: Self::Export);
    /// Applies the VM-wide view and DNS server, with binds retained by the root.
    fn view(
        &mut self,
        root: &mut Self::Root,
        view: &activation::View,
        dns: Option<IpAddr>,
    ) -> Result<MachineId, view::ApplyFailure>;
    /// Tears the root down, including its view binds.
    fn teardown(&mut self, root: Self::Root) -> Result<(), TeardownError>;
}

/// The guest kernel's operations.
struct LiveSystem;

impl System for LiveSystem {
    type Root = Root;
    type Export = SourceRoot;

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

    fn mount_export(
        &mut self,
        root: &mut Root,
        tag: &str,
        access: Access,
    ) -> Result<SourceRoot, root::Error> {
        root.mount_export(tag, access)
    }

    fn close(&mut self, export: SourceRoot) {
        drop(export);
    }

    fn view(
        &mut self,
        root: &mut Root,
        view: &activation::View,
        dns: Option<IpAddr>,
    ) -> Result<MachineId, view::ApplyFailure> {
        view::apply_staged(root, view, dns)
    }

    fn teardown(&mut self, root: Root) -> Result<(), TeardownError> {
        root.teardown()
    }
}

/// The result of applying a record through the system seam.
#[derive(Debug)]
struct Activated<R, E> {
    /// The root, retaining its view binds.
    root: R,
    /// The export descriptor, if any.
    export: Option<E>,
    /// The record's proxy endpoint.
    proxy: Option<String>,
    /// The record's publication, or unpublished without a network.
    publication: Publication,
    /// The template's gateway.
    gateway: Option<Ipv4Addr>,
}

/// The result through a particular system implementation.
type ActivatedBy<S> = Activated<<S as System>::Root, <S as System>::Export>;

/// Applies an accepted record through the system seam.
fn apply_with<S: System>(
    system: &mut S,
    record: &ActivationRecord,
    template: &RootTemplate,
    deadline: Option<Instant>,
) -> Result<ActivatedBy<S>, ApplyError> {
    let configuration = record.configuration();
    let assembly = assembly(template, configuration);
    if let root::Image::Gpt { .. } = assembly.image {
        system
            .reread(assembly.distro, assembly.timeout, deadline)
            .map_err(ApplyError::Gpt)?;
    }
    let mut root = system
        .assemble(&assembly, deadline)
        .map_err(ApplyError::Assembly)?;
    let export = match configuration.export {
        Some(_) => {
            match system.mount_export(&mut root, &template.filesystem_tag, Access::ReadWrite) {
                Ok(export) => Some(export),
                Err(error) => {
                    let teardown = system.teardown(root).err();
                    return Err(ApplyError::Export { error, teardown });
                }
            }
        }
        None => None,
    };
    let network = configuration.network.as_ref();
    let publication = network.map_or(Publication::Unpublished, |network| network.publication);
    if publication == Publication::Published && template.gateway.is_none() {
        let teardown = discard(system, export, root);
        return Err(ApplyError::Network {
            error: NetworkError::Gateway,
            teardown,
        });
    }
    let dns = network.and_then(|network| network.dns);
    if let Err(error) = system.view(&mut root, &configuration.view, dns) {
        let teardown = discard(system, export, root);
        return Err(view_failure(error, teardown));
    }
    Ok(Activated {
        root,
        export,
        proxy: network.and_then(|network| network.proxy.clone()),
        publication,
        gateway: template.gateway,
    })
}

/// Preserves the network classification and original cause of resolver failures.
fn view_failure(failure: view::ApplyFailure, teardown: Option<TeardownError>) -> ApplyError {
    let error = match failure {
        view::ApplyFailure::View(error) => return ApplyError::View { error, teardown },
        view::ApplyFailure::ResolverWrite(error) => NetworkError::Write(error),
        view::ApplyFailure::ResolverBind(error) => NetworkError::Bind(error),
    };
    ApplyError::Network { error, teardown }
}

/// Closes an export descriptor, then tears down its root, returning any teardown failure.
fn discard<S: System>(
    system: &mut S,
    export: Option<S::Export>,
    root: S::Root,
) -> Option<TeardownError> {
    if let Some(export) = export {
        system.close(export);
    }
    system.teardown(root).err()
}

/// Tears down an application through the seam, with bind cleanup owned solely by its root.
fn teardown_with<S: System>(
    system: &mut S,
    export: Option<S::Export>,
    root: S::Root,
) -> Result<(), AppliedTeardownError> {
    match discard(system, export, root) {
        None => Ok(()),
        Some(root) => Err(AppliedTeardownError { root: Some(root) }),
    }
}

/// Combines the record's root settings with the template's devices and runtime directory.
fn assembly(template: &RootTemplate, configuration: &Configuration) -> Assembly {
    let image = match configuration.image {
        activation::Image::Erofs { uuid } => root::Image::Erofs { uuid },
        activation::Image::Gpt {
            disk_guid,
            first_partition_bound,
        } => root::Image::Gpt {
            expected: match disk_guid {
                Some(guid) => Expected::new().disk_guid(guid),
                None => Expected::new(),
            },
            first_partition_bound: usize::try_from(first_partition_bound).unwrap_or(usize::MAX),
        },
    };
    let upper = match configuration.upper {
        activation::Upper::Tmpfs { max_memory_percent } => root::Upper::Tmpfs {
            max_memory_percent: Some(max_memory_percent),
        },
        activation::Upper::Scratch { journal } => root::Upper::Scratch {
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
