//! The application of the record of the Activate that a clone's [`Activation`] accepted:
//! [`apply`](fn@apply), which activates the clone's root and workload with the record's settings,
//! as at a cold boot, as the [module](super::super#activation) describes.

#[cfg(test)]
mod tests;

use std::fmt;
use std::fs;
use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[cfg(doc)]
use super::super::Devices;
#[cfg(doc)]
use super::Activation;
use crate::activation::{
    self, ActivationRecord, Configuration, MachineIdMode, Publication, WorkloadIdentity,
};
use crate::gpt::Expected;
#[cfg(doc)]
use crate::guest::entry::AccountMode;
use crate::guest::entry::{self, Entry, Identity, MachineId, RuntimeBinds};
use crate::guest::ingress::{Announcer, StartError};
use crate::guest::mappings::{self, Access, SourceRoot};
use crate::guest::root::{self, Assembly, AssemblyError, Device, Root, TeardownError};
use crate::protocol::cmdline::write_resolver;

/// The name of the file, in the template's [runtime directory](RootTemplate::runtime), to which
/// [`apply`](fn@apply) writes the machine ID that it generates for a record that asks for one, as
/// NVX's managed agent writes its own to `/run/nvx/workload-machine-id`.
pub const MACHINE_ID_FILE: &str = "workload-machine-id";

/// The name of the file, in the template's [runtime directory](RootTemplate::runtime), to which
/// [`apply`](fn@apply) writes the resolver file that names the DNS server of a record's network,
/// which every execution sees read-only at `/etc/resolv.conf`.
pub const RESOLVER_FILE: &str = "resolv.conf";

/// The mode of the resolver file, whatever the umask: every execution may read it.
const RESOLVER_MODE: u32 = 0o444;

/// The inputs of a clone's activation that its guest takes from its own configuration, which its
/// template captured, so that every clone of the template has the same: the runtime directory,
/// the block devices, which their MMIO windows name, with the sizes that they must have, how long
/// the assembly of the root waits for each, the host filesystem device, which its virtio-fs tag
/// names, and the IPv4 gateway of the template's NIC, if it has one. [`apply`](fn@apply) takes
/// the rest from the activation record.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RootTemplate {
    /// The runtime directory, below which the root is mounted, as [`Assembly::runtime`] tells,
    /// and in which the application writes a machine ID that it generates, as
    /// [`MACHINE_ID_FILE`], and a resolver file, as [`RESOLVER_FILE`].
    pub runtime: PathBuf,
    /// The `distro` block device, the template's placeholder, which holds each clone's image.
    pub distro: Device,
    /// The scratch block device, which holds the upper of a record whose upper is the scratch,
    /// and which a record whose upper is a tmpfs leaves alone.
    pub scratch: Device,
    /// How long the assembly waits for each device to appear, until the deadline that
    /// [`apply`](fn@apply) takes at the latest.
    pub timeout: Duration,
    /// The virtio-fs tag of the host filesystem device, from which the template's capture found
    /// nothing mounted, as [`Devices::filesystem_tag`] names it, and through which the host
    /// attaches the export of a record that has one.
    pub filesystem_tag: String,
    /// The IPv4 gateway of the template's NIC, which the template's kernel command line names, as
    /// [`gateway`](crate::protocol::cmdline::gateway) reads it, if the template has a NIC: the
    /// gateway to which the announcer of the workload's listeners sends its announcements, for a
    /// record that publishes them.
    pub gateway: Option<Ipv4Addr>,
}

/// A clone's root and workload, which [`apply`](fn@apply) activated: the [`Root`] that it
/// assembled, which owns the root's mounts, the host export's among them, the root of the host
/// export, if the record has one, the runtime's binds that it made on the root, in the caller's
/// mount namespace, the identity in which every execution runs, if the record has a workload
/// identity, the workload's hostname and machine ID, the record's, the resolver file that names
/// the DNS server of the record's network, if it names one, the endpoint of its proxy, if any, and
/// the announcer of the workload's listeners, if the record's network publishes them.
///
/// Without a workload identity, the adapter gives each execution its own identity, which
/// [`Entry::check`] checks when the adapter prepares the execution's entry, as at a cold boot.
/// Every execution's entry takes the [hostname](Self::hostname), through [`Entry::hostname`], and
/// sees the [machine ID](Self::machine_id) and the [resolver file](Self::resolver) through the
/// runtime's binds, which each execution's mount namespace copies from the caller's. The adapter
/// [takes](Self::take_export) the export's root, from which it maps the host's mapping table once
/// the clone is ready, and tells its workloads the [proxy's endpoint](Self::proxy) as at a cold
/// boot. The adapter watches the [announcer](Self::announcer) through its pidfd, and must end the
/// clone once the announcer ended: the gateway then keeps publishing the listeners that the
/// announcer last announced until the VM ends, and learns of no later change of them.
/// [`teardown`](Self::teardown) stops the announcer, closes the export's root, if the adapter did
/// not take it, removes the binds, then tears the root down, and dropping it does the same,
/// ignoring failures, as dropping a [`Root`] does.
#[derive(Debug)]
pub struct Applied {
    /// The announcer of the workload's listeners, if the record's network publishes them, which a
    /// drop stops first, as a teardown does.
    announcer: Option<Announcer>,
    /// The root of the host export, which keeps the export's mount busy, so that a drop closes it
    /// before the root drops, unless the adapter took it.
    export: Option<SourceRoot>,
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
    /// The resolver file that every execution sees at `/etc/resolv.conf`, if the record's
    /// network names a DNS server.
    resolver: Option<PathBuf>,
    /// The endpoint of the proxy through which the workload reaches the network, if the record's
    /// network names one.
    proxy: Option<String>,
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

    /// Returns the resolver file, of [`RESOLVER_FILE`], that names the DNS server of the record's
    /// network, which the application wrote once, and bound read-only over the root's
    /// `/etc/resolv.conf`, for every execution. Returns `None` if the record's network names no
    /// DNS server, or the record has no network, so that every execution sees the image's own
    /// `/etc/resolv.conf`.
    pub fn resolver(&self) -> Option<&Path> {
        self.resolver.as_deref()
    }

    /// Returns the endpoint of the proxy through which the workload reaches the network, as the
    /// guest sees it, if the record's network names one. The application leaves it to the
    /// adapter, which tells its workloads as at a cold boot.
    pub fn proxy(&self) -> Option<&str> {
        self.proxy.as_deref()
    }

    /// Returns the announcer of the workload's listeners, which the application started, if the
    /// record's network publishes them, so that the adapter polls its [pidfd](Announcer::pidfd),
    /// and ends the clone once the announcer ended, as it must: the gateway then keeps publishing
    /// the listeners that the announcer last announced, and learns of no later change of them.
    pub fn announcer(&self) -> Option<&Announcer> {
        self.announcer.as_ref()
    }

    /// Returns the announcer of the workload's listeners, as [`announcer`](Self::announcer)
    /// does, so that the adapter collects its status once it ended.
    pub fn announcer_mut(&mut self) -> Option<&mut Announcer> {
        self.announcer.as_mut()
    }

    /// Takes the root of the host export, if the record has an export, and the adapter did not
    /// take it before: the [`SourceRoot`] from which the adapter maps the host's mapping table
    /// with [`Root::map`] once the clone is ready, as at a cold boot.
    ///
    /// The export's root keeps the export's mount busy, so the adapter drops it before the
    /// [teardown](Self::teardown), which otherwise fails to unmount the export, and leaves it
    /// mounted, as [`Root::mount_export`] tells.
    pub fn take_export(&mut self) -> Option<SourceRoot> {
        self.export.take()
    }

    /// Stops the announcer of the workload's listeners, if the application started one, as
    /// [`Announcer::stop`] does, ignoring its failure, closes the root of the host export, unless
    /// the adapter [took](Self::take_export) it, removes the runtime's binds, then tears the root
    /// down, as [`Root::teardown`] does, which unmounts the host export too, in the caller's mount
    /// namespace, which must be the one where the application made the binds: the export's root
    /// and the binds keep the root's mounts busy, and a teardown detaches no busy mount.
    ///
    /// It attempts both the removal and the teardown, whatever failed before, and fails with the
    /// binds that stay mounted, and the unmounts of the root's teardown that failed, whose mounts
    /// stay.
    pub fn teardown(self) -> Result<(), AppliedTeardownError> {
        let Self {
            export,
            mut binds,
            root,
            announcer,
            ..
        } = self;
        teardown_with(&mut LiveSystem, announcer, export, binds.take(), root)
    }

    /// Returns the assembled root, the runtime's binds on it, and the identity in which every
    /// execution runs, if any, for an adapter that keeps them apart: it then removes the binds
    /// with [`RuntimeBinds::remove`] before it tears the root down, in the same mount namespace,
    /// since dropping them leaves them mounted. It stops the announcer of the workload's
    /// listeners, if the application started one, and closes the root of the host export, unless
    /// the adapter [took](Self::take_export) it.
    pub fn into_parts(self) -> (Root, RuntimeBinds, Option<Identity>) {
        let Self {
            export,
            mut binds,
            root,
            identity,
            announcer,
            ..
        } = self;
        if let Some(announcer) = announcer {
            let _ = announcer.stop();
        }
        drop(export);
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
///    hostname and machine-ID mode, whether it has an export, and its network's DNS server,
///    proxy endpoint, and publication of the workload's listeners from the record too;
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
/// 6. if the record has an export, mounts the host export, through the template's host filesystem
///    device, which [`RootTemplate::filesystem_tag`] names, with [`Root::mount_export`], whatever
///    the export's shape: at `hostfs/root` in the runtime directory, outside the root, where only
///    the guest's root user reaches it, read-write. The record names no mode of the mount, and none
///    is needed: OpenVMM limits each of the export's children to its own access, and each mapping
///    takes its own, so the mount only lets the read-write children through. OpenVMM resolves an
///    export's path again at each launch and restore, so the directories above an export's
///    directory must be ones that no guest may change. Whoever names an export must not name one
///    inside a directory that a writable export of another sandbox or clone holds. A failure fails
///    with [`ApplyError::Export`], once the application tore the root down, which unmounts an
///    export that it mounted;
/// 7. if the record's network names a DNS server, writes a resolver file that names it, as
///    [`write_resolver`] writes one, to [`RESOLVER_FILE`] in the template's runtime directory: a
///    new file, of mode 0444, whatever the umask, so that every execution may read it. A file
///    there already, or one that cannot be created or written, fails with
///    [`ApplyError::Network`], once the application tore the root down;
/// 8. if the record's network publishes the workload's listeners, starts their announcer, to
///    the gateway of [`RootTemplate::gateway`], with [`Announcer::start`]: it binds a UDP socket
///    to a privileged source port, and spawns `/sbin/nvx-ingress inherit GATEWAY_IPV4` with the
///    socket as its standard input, and waits until the program runs, but for no announcement.
///    The template captured its NIC under a policy that publishes nothing, so no announcer ran
///    in it. A template without a gateway, a socket that cannot be bound, and a program that does
///    not run fail with [`ApplyError::Network`], once the application tore the root down;
/// 9. binds the runtime's files into the root, for every execution, with one
///    [`Entry::bind_runtime`], in the caller's mount namespace, of which each execution's starts
///    as a copy: a generated machine ID's file, read-only over the root's `/etc/machine-id`, then
///    the resolver file, read-only over the root's `/etc/resolv.conf`, and, with the image's own
///    machine ID and without a resolver file, nothing. The entry checks the record's hostname
///    first, which every execution's entry then takes, through [`Entry::hostname`]. A failure to
///    bind the resolver file fails with [`ApplyError::Network`], and any other with
///    [`ApplyError::View`], once the binds that it made were removed, the application stopped
///    the announcer that it started, and tore the root down.
///
/// The activation answers a retry of the Activate without applying the record again, so every
/// execution, and every retry, sees the same machine ID and resolver file. A failure after the
/// generation, or after the resolver file's write, leaves that file, since the activation fails
/// finally, and the adapter ends the VM.
///
/// It returns the root of the host export, the [`SourceRoot`] from which the adapter maps the
/// host's mapping table once the clone is ready, as at a cold boot, the endpoint of the network's
/// proxy, which it leaves to the adapter, which tells its workloads as at a cold boot, and the
/// announcer that it started, which the adapter watches. It starts no other process, generates at
/// most one ID, after the activation accepted the record, and never powers the VM off: a failure,
/// which the adapter reports to [`Activation::complete`], fails the activation finally, and the
/// adapter ends the VM.
///
/// It needs the guest's root user, in the guest's initial mount namespace, as
/// [`root::assemble`] does, and the binds of a generated machine ID and of a resolver file need
/// Linux 6.8 or later, as [`Entry::bind_runtime`] does.
pub fn apply(
    record: &ActivationRecord,
    template: &RootTemplate,
    deadline: Option<Instant>,
) -> Result<Applied, ApplyError> {
    let activated = apply_with(&mut LiveSystem, record, template, deadline)?;
    Ok(Applied {
        export: activated.export,
        binds: Binds(Some(activated.binds)),
        root: activated.root,
        identity: activated.identity,
        hostname: activated.hostname,
        machine_id: activated.machine_id,
        resolver: activated.resolver,
        proxy: activated.proxy,
        announcer: activated.announcer,
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
    /// The host export could not be mounted, as this error tells: the template's host filesystem
    /// device has no export attached, or the root refused the mount. The application then tore
    /// the root down, which unmounts the export if it was mounted.
    Export {
        /// Why the export could not be mounted, as [`Root::mount_export`] tells.
        error: root::Error,
        /// The unmounts of the root's teardown that failed, if any, whose mounts stay.
        teardown: Option<TeardownError>,
    },
    /// The record's network settings could not be applied, as this error tells: the resolver
    /// file that names the network's DNS server could not be written, or bound over the root's
    /// `/etc/resolv.conf`, or the announcer of the workload's listeners, which the network
    /// publishes, could not start. The application then stopped an announcer that it started,
    /// and tore the root down.
    Network {
        /// Why the network settings could not be applied.
        error: NetworkError,
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
            Self::Account { error, .. } | Self::View { error, .. } => Some(error),
            Self::MachineId { error, .. } => Some(error),
            Self::Network { error, .. } => Some(error),
        }
    }
}

/// Why the network settings of a record could not be applied, which an [`ApplyError::Network`]
/// holds.
#[derive(Debug)]
#[non_exhaustive]
pub enum NetworkError {
    /// The resolver file that names the network's DNS server could not be written to
    /// [`RESOLVER_FILE`] in the template's runtime directory, as this error tells: a file is
    /// there already, or the file cannot be created or written.
    Write(io::Error),
    /// The resolver file could not be bound over the root's `/etc/resolv.conf`, as
    /// [`Entry::bind_runtime`] tells: with an [`entry::Error::Resolver`], or an
    /// [`entry::Error::Rollback`] of one, after which the entry removed the binds that it made,
    /// unless the error names those that stay.
    Bind(entry::Error),
    /// The network publishes the workload's listeners, but the template names no IPv4 gateway,
    /// [`RootTemplate::gateway`], to which their announcer would send its announcements.
    Gateway,
    /// The announcer of the workload's listeners, which the network publishes, could not start,
    /// as [`Announcer::start`] tells.
    Announcer(StartError),
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
            Self::Announcer(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for NetworkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Write(error) => Some(error),
            // The entry's error names the resolver file already, so it stands for this one, as
            // the start's error names the announcer.
            Self::Bind(error) => error.source(),
            Self::Announcer(error) => error.source(),
            Self::Gateway => None,
        }
    }
}

/// The calls through which an application reaches the system: the guest kernel's, in a VM, and a
/// fake's, which records them, in tests.
trait System {
    /// What an assembly mounted: a [`Root`], in a VM.
    type Root;

    /// The root of a host export that a mount returned: a [`SourceRoot`], in a VM.
    type Export;

    /// The runtime's binds that a bind made: [`RuntimeBinds`], in a VM.
    type Binds;

    /// The announcer of the workload's listeners that a start returned: an [`Announcer`], in a
    /// VM.
    type Announcer;

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

    /// Mounts the host export, the virtio-fs device `tag`, with `access`, for `root`, and returns
    /// the export's root, as [`Root::mount_export`] does.
    fn mount_export(
        &mut self,
        root: &mut Self::Root,
        tag: &str,
        access: Access,
    ) -> Result<Self::Export, root::Error>;

    /// Closes `export`, the root of a host export, which keeps the export's mount busy until then.
    fn close(&mut self, export: Self::Export);

    /// Writes a resolver file that names `server` to a new file at `path`, as
    /// [`write_resolver_file`] does.
    fn write_resolver(&mut self, path: &Path, server: IpAddr) -> io::Result<()>;

    /// Starts the announcer of the workload's listeners, which announces them to `gateway`, as
    /// [`Announcer::start`] does.
    fn announce(&mut self, gateway: Ipv4Addr) -> Result<Self::Announcer, StartError>;

    /// Stops `announcer`, as [`Announcer::stop`] does, ignoring its failure.
    fn stop(&mut self, announcer: Self::Announcer);

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
    type Export = SourceRoot;
    type Binds = RuntimeBinds;
    type Announcer = Announcer;

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

    fn write_resolver(&mut self, path: &Path, server: IpAddr) -> io::Result<()> {
        write_resolver_file(path, server)
    }

    fn announce(&mut self, gateway: Ipv4Addr) -> Result<Announcer, StartError> {
        Announcer::start(gateway)
    }

    fn stop(&mut self, announcer: Announcer) {
        let _ = announcer.stop();
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
/// identity of the entry through which it binds the runtime's files, the hostname, the machine
/// ID, and the resolver file.
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
    /// The resolver file that every execution sees at `/etc/resolv.conf`, if the record's
    /// network names a DNS server.
    resolver: Option<PathBuf>,
}

impl View {
    /// Returns the entry into the root at `root` that binds the view's runtime files, and checks
    /// its hostname, with [`Entry::bind_runtime`].
    fn entry(&self, root: &Path) -> Entry {
        let entry = Entry::new(root, self.identity.clone())
            .hostname(self.hostname.clone())
            .machine_id(self.machine_id.clone());
        match &self.resolver {
            Some(resolver) => entry.resolver(resolver.clone()),
            None => entry,
        }
    }
}

/// What an application activated through a system, which an [`Applied`] holds in a VM: the
/// root, the root of the host export, if any, the runtime's binds on the root, the identity in
/// which every execution runs, if any, the workload's hostname and machine ID, the resolver file,
/// if any, the proxy's endpoint, if any, and the announcer of the workload's listeners, if any.
#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
struct Activated<R, E, B, A> {
    /// The assembled root.
    root: R,
    /// The root of the host export, if the record has an export.
    export: Option<E>,
    /// The runtime's binds on the root.
    binds: B,
    /// The identity in which every execution runs, if the record has a workload identity.
    identity: Option<Identity>,
    /// The hostname of every execution.
    hostname: Vec<u8>,
    /// The machine ID that every execution sees.
    machine_id: MachineId,
    /// The resolver file that every execution sees, if the record's network names a DNS server.
    resolver: Option<PathBuf>,
    /// The endpoint of the proxy, if the record's network names one.
    proxy: Option<String>,
    /// The announcer of the workload's listeners, if the record's network publishes them.
    announcer: Option<A>,
}

/// What an application through a system of type `S` activated.
type ActivatedBy<S> = Activated<
    <S as System>::Root,
    <S as System>::Export,
    <S as System>::Binds,
    <S as System>::Announcer,
>;

/// Applies `record` with `template` by `deadline`, if any, through `system`, as
/// [`apply`](fn@apply) does, and returns what it activated.
fn apply_with<S: System>(
    system: &mut S,
    record: &ActivationRecord,
    template: &RootTemplate,
    deadline: Option<Instant>,
) -> Result<ActivatedBy<S>, ApplyError> {
    let configuration = record.configuration();
    let assembly = assembly(template, configuration);
    let workload = &configuration.workload;
    let identity = workload.identity.map(workload_identity);
    if let root::Image::Gpt { .. } = assembly.image {
        system
            .reread(assembly.distro, assembly.timeout, deadline)
            .map_err(ApplyError::Gpt)?;
    }
    let mut root = system
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
    // The record names no mode of the export's mount: OpenVMM bounds each child's access.
    let export = match configuration.export {
        Some(_) => {
            let tag = &template.filesystem_tag;
            match system.mount_export(&mut root, tag, Access::ReadWrite) {
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
    let resolver = match network.and_then(|network| network.dns) {
        Some(server) => {
            let path = template.runtime.join(RESOLVER_FILE);
            if let Err(error) = system.write_resolver(&path, server) {
                let teardown = discard(system, export, root);
                let error = NetworkError::Write(error);
                return Err(ApplyError::Network { error, teardown });
            }
            Some(path)
        }
        None => None,
    };
    let publishes = network.is_some_and(|network| network.publication == Publication::Published);
    let announcer = if publishes {
        let started = match template.gateway {
            Some(gateway) => system.announce(gateway).map_err(NetworkError::Announcer),
            None => Err(NetworkError::Gateway),
        };
        match started {
            Ok(announcer) => Some(announcer),
            Err(error) => {
                let teardown = discard(system, export, root);
                return Err(ApplyError::Network { error, teardown });
            }
        }
    } else {
        None
    };
    let view = View {
        identity: identity.clone().unwrap_or_else(|| Identity::new(0, 0)),
        hostname: workload.hostname.clone(),
        machine_id,
        resolver,
    };
    let binds = match system.bind(&root, &view) {
        Ok(binds) => binds,
        Err(error) => {
            if let Some(announcer) = announcer {
                system.stop(announcer);
            }
            let teardown = discard(system, export, root);
            return Err(if is_resolver(&error) {
                let error = NetworkError::Bind(error);
                ApplyError::Network { error, teardown }
            } else {
                ApplyError::View { error, teardown }
            });
        }
    };
    Ok(Activated {
        root,
        export,
        binds,
        identity,
        hostname: view.hostname,
        machine_id: view.machine_id,
        resolver: view.resolver,
        proxy: network.and_then(|network| network.proxy.clone()),
        announcer,
    })
}

/// Closes `export`, the root of the host export, if any, then tears `root` down, through
/// `system`, once an application failed, and returns the unmounts of the teardown that failed, if
/// any.
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

/// Returns whether `error`, why [`Entry::bind_runtime`] failed, is the failure of the resolver
/// file's bind, possibly with binds that stay mounted.
fn is_resolver(error: &entry::Error) -> bool {
    match error {
        entry::Error::Resolver(_) => true,
        entry::Error::Rollback { error, .. } => is_resolver(error),
        _ => false,
    }
}

/// Stops `announcer`, if any, closes `export`, the root of the host export, if any, removes
/// `binds`, then tears `root` down, through `system`, as [`Applied::teardown`] does, attempting the
/// removal and the teardown whatever failed before.
fn teardown_with<S: System>(
    system: &mut S,
    announcer: Option<S::Announcer>,
    export: Option<S::Export>,
    binds: S::Binds,
    root: S::Root,
) -> Result<(), AppliedTeardownError> {
    if let Some(announcer) = announcer {
        system.stop(announcer);
    }
    if let Some(export) = export {
        system.close(export);
    }
    let binds = system.remove(binds).err();
    let root = system.teardown(root).err();
    if binds.is_none() && root.is_none() {
        return Ok(());
    }
    Err(AppliedTeardownError { binds, root })
}

/// Writes a resolver file that names `server` as the guest's one DNS server, as [`write_resolver`]
/// writes one, to a new file at `path`, with mode 0444 whatever the process's umask, so that
/// every execution may read it.
///
/// A file, or a symbolic link, at `path` already fails the call with `AlreadyExists`, and is left
/// alone, so each activation writes its resolver file once.
fn write_resolver_file(path: &Path, server: IpAddr) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(RESOLVER_MODE)
        .open(path)?;
    // The umask narrows the mode that creates the file, which could keep executions of other
    // users from reading it.
    file.set_permissions(fs::Permissions::from_mode(RESOLVER_MODE))?;
    write_resolver(&mut file, server)
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
