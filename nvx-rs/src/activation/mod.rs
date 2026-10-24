//! The activation record of a layered root: one bounded, versioned, non-secret record of the
//! settings that a root's activation applies, and what Ready reports about that activation, which
//! the host, the guest, and every adapter's framing share.
//!
//! A layered root's guest assembles its root and enters its workload with settings that the host
//! chooses for each launch. A clone that the host restores from a template keeps the template's
//! captured configuration, so its record gives it the settings of its own launch: everything that
//! it needs to activate. A cold boot's record holds the settings that its guest applied, so that
//! every layered root reports the identity of its configuration in the same way:
//!
//! - [`ActivationRecord`]: the [`VERSION`] of its schema, the [`RestoreSection`] of a clone, and
//!   the [`Configuration`]. A clone's restore section holds the restore's [`TransactionId`], 16
//!   random bytes that the host picks for each restore and keeps across the restore's retries,
//!   and the [`TemplateReference`] of the template that the clone restores: the template's
//!   [`StoreGeneration`], the nonzero publication ID that its store assigned to it, and the
//!   [`Identity`] of its key. A cold boot's record has no restore section.
//! - [`Configuration`]: the non-secret settings that the activation applies, which the
//!   [configuration](#configuration) lists.
//! - [`ReadyActivation`]: what Ready reports about the activation: the [`ConfigurationIdentity`],
//!   which every layered root reports, and the transaction ID, which only a clone reports.
//!
//! [`ActivationRecord::new`] admits a configuration only once [`Configuration::check`] finds that
//! it keeps every [limit](#limits), so that every record holds settings that a guest can apply.
//! The module depends on nothing outside the standard library and the crate's `protocol` and
//! `uuid` modules, so it builds without features, and the host, the guest, and every adapter
//! share it.
//!
//! # Configuration
//!
//! A [`Configuration`] holds:
//!
//! - the [`Image`]: its format, an EROFS filesystem or a GPT disk, the [`Identity`] of the
//!   image's binding, and the EROFS filesystem's UUID or the GPT disk's GUID that the guest
//!   expects, if any;
//! - the [`Upper`]: a tmpfs, or the scratch, with the [`Identity`] of the scratch's binding;
//! - the [`Export`], if any: one export, a directory or an aggregate, with the [`Identity`] of
//!   its plan, which covers the export's whole shape, and the number of host mappings that the
//!   host sends once the guest is ready, from 1 to [`MAX_MAPPINGS`]. A configuration without an
//!   export has no mappings;
//! - the [`Network`] of a VM that has one: the [`Identity`] of its policy, the address of its DNS
//!   server, if any, the [`Publication`] of the workload's listeners, and the endpoint of its
//!   proxy, if any, which holds no credentials;
//! - the [`WorkloadView`]: the hostname, the [`MachineIdMode`], the image's own machine ID or one
//!   that the guest generates, and the [`WorkloadIdentity`], if any: a UID and a GID, neither 0
//!   nor `u32::MAX`, whose account the image must hold, without supplementary groups or
//!   capabilities, in which every execution runs, as each execution of a layered sandbox runs
//!   today. An adapter whose executions name their own identity, as CRI containers do, leaves it
//!   out;
//! - the root's settings: the [`OverlayMode`], the [`Journal`] policy of a scratch upper, and the
//!   bound of a GPT disk's first partition;
//! - the [`ExpectedResources`]: the CPU count and the memory size that the guest must have, as a
//!   clone's repair produces them.
//!
//! The guest takes the other inputs of its root's assembly from its own configuration, which a
//! template captures, so that they are invariants of the template: the runtime directory, the
//! MMIO windows and the sizes of the block devices, the partitions that a GPT disk must have, of
//! which it expects none, and how long it waits for the devices.
//!
//! Secrets, per-execution environments, and host mapping tables never enter a record: they
//! travel on the control channel once the guest is ready, as after a cold boot.
//!
//! # Limits
//!
//! The limits are public constants: a record holds at most [`MAX_RECORD_BYTES`] bytes, so that a
//! restore packet could carry the same bytes, a configuration announces at most [`MAX_MAPPINGS`]
//! mappings, a hostname holds at most [`MAX_HOSTNAME_BYTES`] bytes, and a proxy endpoint at most
//! [`MAX_PROXY_ENDPOINT_BYTES`]. The limits of a record's parts keep every record within
//! [`MAX_RECORD_BYTES`].
//!
//! [`Configuration::check`] also refuses an export without mappings, mappings without an export,
//! a workload identity whose UID or GID is 0, or `u32::MAX`, which the kernel reads as an
//! unchanged ID, a hostname with a NUL, a proxy endpoint with anything other than visible ASCII
//! characters or with an `@`, which would start credentials, and a zero CPU count, memory size,
//! or bound of a GPT disk's first partition.
//!
//! ```
//! use std::num::NonZeroU64;
//!
//! use nvx::activation::{
//!     ActivationRecord, Configuration, ExpectedResources, Identity, Image, MachineIdMode,
//!     OverlayMode, RestoreSection, StoreGeneration, TemplateReference, TransactionId, Upper,
//!     WorkloadIdentity, WorkloadView,
//! };
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let configuration = Configuration {
//!     image: Image::Erofs {
//!         binding: Identity::from_bytes([0x11; 32]),
//!         uuid: Some("0123abcd-4567-89ef-0123-456789abcdef".parse()?),
//!     },
//!     upper: Upper::Tmpfs,
//!     overlay: OverlayMode::UntrustedLowers,
//!     export: None,
//!     mappings: 0,
//!     network: None,
//!     workload: WorkloadView {
//!         hostname: b"sandbox".to_vec(),
//!         machine_id: MachineIdMode::Generated,
//!         identity: Some(WorkloadIdentity { uid: 1000, gid: 1000 }),
//!     },
//!     resources: ExpectedResources { cpus: 2, memory: 1 << 30 },
//! };
//! // A cold boot's record has no restore section.
//! let cold = ActivationRecord::new(None, configuration.clone())?;
//! assert!(cold.restore().is_none());
//! // A clone's names its restore's transaction and the template that it restores.
//! let restore = RestoreSection {
//!     transaction: TransactionId::from_bytes([0x22; 16]),
//!     template: TemplateReference {
//!         generation: StoreGeneration::new(NonZeroU64::MIN),
//!         key: Identity::from_bytes([0x44; 32]),
//!     },
//! };
//! let clone = ActivationRecord::new(Some(restore), configuration)?;
//! assert_eq!(clone.restore(), Some(&restore));
//! # Ok(())
//! # }
//! ```

use std::fmt;
use std::net::IpAddr;
use std::num::NonZeroU64;

use crate::protocol::MAX_HOST_MAPPINGS;
use crate::protocol::root::ImageFormat;
use crate::uuid::Uuid;

#[cfg(test)]
mod tests;

/// The version of the activation contract that this build speaks: the schema of an
/// [`ActivationRecord`], which every record names. A record of another version is refused.
pub const VERSION: u16 = 1;

/// The most bytes that a record holds, so that a restore packet could carry the same bytes.
pub const MAX_RECORD_BYTES: usize = 1024;

/// The most host mappings that a configuration announces: as many as a host mapping table holds,
/// [`MAX_HOST_MAPPINGS`].
pub const MAX_MAPPINGS: usize = MAX_HOST_MAPPINGS;

/// The longest hostname, in bytes: Linux's `HOST_NAME_MAX`, which the guest's entry applies.
pub const MAX_HOSTNAME_BYTES: usize = 64;

/// The longest proxy endpoint, in bytes.
pub const MAX_PROXY_ENDPOINT_BYTES: usize = 255;

/// The identity of an object that a record names: 32 bytes that the host computes, such as a
/// SHA-256 digest of the object's canonical form, by which a record names the binding of its
/// image and of its scratch, the plan of its export, the policy of its network, and the key of a
/// clone's template. A record never holds or reads the objects themselves.
///
/// It displays as 64 lowercase hexadecimal digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Identity([u8; 32]);

impl Identity {
    /// The length of an identity, in bytes.
    pub const LEN: usize = 32;

    /// Returns the identity whose bytes are `bytes`.
    pub const fn from_bytes(bytes: [u8; Self::LEN]) -> Self {
        Self(bytes)
    }

    /// Returns the identity's bytes.
    pub const fn as_bytes(&self) -> &[u8; Self::LEN] {
        &self.0
    }
}

impl fmt::Display for Identity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(formatter, &self.0)
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Identity({self})")
    }
}

/// The transaction ID of a clone's restore: 16 random bytes that the host picks for each restore
/// and keeps across the restore's retries, which the clone's Ready reports.
///
/// It displays as 32 lowercase hexadecimal digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TransactionId([u8; 16]);

impl TransactionId {
    /// The length of a transaction ID, in bytes.
    pub const LEN: usize = 16;

    /// Returns the transaction ID whose bytes are `bytes`.
    pub const fn from_bytes(bytes: [u8; Self::LEN]) -> Self {
        Self(bytes)
    }

    /// Returns the transaction ID's bytes.
    pub const fn as_bytes(&self) -> &[u8; Self::LEN] {
        &self.0
    }
}

impl fmt::Display for TransactionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(formatter, &self.0)
    }
}

impl fmt::Debug for TransactionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "TransactionId({self})")
    }
}

/// The generation of a template in its store: the template's publication ID, a nonzero number
/// that the store assigns to each template that it publishes, in the order of publication, and
/// never reuses.
///
/// Two generations of one store compare in the order of their publication. A generation displays
/// as its decimal number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StoreGeneration(NonZeroU64);

impl StoreGeneration {
    /// Returns the store generation `generation`.
    pub const fn new(generation: NonZeroU64) -> Self {
        Self(generation)
    }

    /// Returns the store generation's number.
    pub const fn get(self) -> NonZeroU64 {
        self.0
    }
}

impl fmt::Display for StoreGeneration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// The identity of the configuration that a layered root's activation applied, which its Ready
/// reports: 32 bytes.
///
/// It displays as 64 lowercase hexadecimal digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConfigurationIdentity([u8; 32]);

impl ConfigurationIdentity {
    /// The length of a configuration identity, in bytes.
    pub const LEN: usize = 32;

    /// Returns the configuration identity whose bytes are `bytes`.
    pub const fn from_bytes(bytes: [u8; Self::LEN]) -> Self {
        Self(bytes)
    }

    /// Returns the configuration identity's bytes.
    pub const fn as_bytes(&self) -> &[u8; Self::LEN] {
        &self.0
    }
}

impl fmt::Display for ConfigurationIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(formatter, &self.0)
    }
}

impl fmt::Debug for ConfigurationIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ConfigurationIdentity({self})")
    }
}

/// The template that a clone restores: the generation of the template in its store, and the
/// identity of the template's key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TemplateReference {
    /// The template's generation in its store.
    pub generation: StoreGeneration,
    /// The identity of the template's key.
    pub key: Identity,
}

/// The restore section of a clone's record: the restore's transaction ID, and the template that
/// the clone restores.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RestoreSection {
    /// The restore's transaction ID, which the clone's Ready reports.
    pub transaction: TransactionId,
    /// The template that the clone restores.
    pub template: TemplateReference,
}

/// The image of a layered root, on its `distro` block device, in the format that the record
/// names: the guest never probes the image for another.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Image {
    /// An EROFS filesystem, the overlay's one lower layer.
    Erofs {
        /// The identity of the image's binding.
        binding: Identity,
        /// The UUID that the filesystem's superblock must hold, if the guest expects one.
        uuid: Option<Uuid>,
    },
    /// A GPT disk, whose first partition holds opaque bytes, and whose later partitions hold the
    /// overlay's lower layers.
    Gpt {
        /// The identity of the image's binding.
        binding: Identity,
        /// The GUID that the disk must have, if the guest expects one.
        disk_guid: Option<Uuid>,
        /// The most bytes that the disk's first partition may hold: at least 1, since a
        /// partition holds at least one block.
        first_partition_bound: u64,
    },
}

impl Image {
    /// Returns the image's format.
    pub fn format(&self) -> ImageFormat {
        match self {
            Self::Erofs { .. } => ImageFormat::Erofs,
            Self::Gpt { .. } => ImageFormat::Gpt,
        }
    }

    /// Returns the identity of the image's binding.
    pub fn binding(&self) -> &Identity {
        match self {
            Self::Erofs { binding, .. } | Self::Gpt { binding, .. } => binding,
        }
    }

    /// Returns what the guest expects of the image, if anything: the EROFS filesystem's UUID, or
    /// the GPT disk's GUID.
    pub fn expected(&self) -> Option<Uuid> {
        match self {
            Self::Erofs { uuid, .. } => *uuid,
            Self::Gpt { disk_guid, .. } => *disk_guid,
        }
    }
}

/// The upper of a layered root, which holds the overlay's upper and work directories.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Upper {
    /// A tmpfs, for a root that keeps no upper on a scratch.
    Tmpfs,
    /// A whole-device ext4 filesystem on the `scratch` block device.
    Scratch {
        /// The identity of the scratch's binding.
        binding: Identity,
        /// Whether the scratch's filesystem may have a journal.
        journal: Journal,
    },
}

/// Whether an ext4 scratch may have a journal: the guest's assembly refuses a scratch with one
/// when the policy [forbids](Self::Forbidden) it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Journal {
    /// The scratch may have a journal or none.
    Allowed,
    /// The scratch may not have a journal.
    Forbidden,
}

/// The overlay's mode, which selects its one option set, as the guest's assembly names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OverlayMode {
    /// `metacopy=on,xino=on`, for lowers whose overlay xattrs the guest trusts.
    TrustedLowers,
    /// `metacopy=off,redirect_dir=nofollow`, for lowers that may carry untrusted overlay xattrs.
    UntrustedLowers,
}

/// The shape of an export.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExportKind {
    /// One host directory, which is the export's root.
    Directory,
    /// An aggregate of host directories and files, each a child of the export's root.
    Aggregate,
}

/// The export that the guest mounts as the source of the host's mappings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Export {
    /// The export's shape.
    pub kind: ExportKind,
    /// The identity of the export's plan, which covers the export's whole shape, so that an
    /// export of any shape has a record of the same form.
    pub plan: Identity,
}

/// Whether the workload's listeners are published, through private-network ingress or the
/// host's loopback, in the vocabulary of the guest's entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Publication {
    /// The workload's listeners are published.
    Published,
    /// No listener is published.
    Unpublished,
}

/// The network of a VM that has one.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Network {
    /// The identity of the network's policy.
    pub policy: Identity,
    /// The address of the DNS server that the guest's resolver configuration names, if any.
    pub dns: Option<IpAddr>,
    /// Whether the workload's listeners are published.
    pub publication: Publication,
    /// The endpoint of the proxy through which the workload reaches the network, as the guest
    /// sees it, such as `http://10.0.0.1:3128`, if any: 1 to [`MAX_PROXY_ENDPOINT_BYTES`] visible
    /// ASCII characters, without an `@`, so that it holds no credentials.
    pub proxy: Option<String>,
}

/// The machine ID that the workload sees at `/etc/machine-id`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MachineIdMode {
    /// The image's own machine ID, which the guest leaves untouched.
    Image,
    /// A machine ID that the guest generates for the activation.
    Generated,
}

/// The identity in which every execution of a workload runs: a UID and a GID, neither 0 nor
/// `u32::MAX`, whose account the image must hold, without supplementary groups or capabilities.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WorkloadIdentity {
    /// The user ID: neither 0 nor `u32::MAX`.
    pub uid: u32,
    /// The group ID: neither 0 nor `u32::MAX`.
    pub gid: u32,
}

/// The view that the workload has of its VM.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WorkloadView {
    /// The hostname: 1 to [`MAX_HOSTNAME_BYTES`] bytes, without a NUL.
    pub hostname: Vec<u8>,
    /// The machine ID that the workload sees.
    pub machine_id: MachineIdMode,
    /// The identity in which every execution runs, unless the executions name their own.
    pub identity: Option<WorkloadIdentity>,
}

/// The resources that the guest must have, as a clone's repair produces them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ExpectedResources {
    /// The number of online CPUs: at least 1.
    pub cpus: u32,
    /// The memory size, in bytes: at least 1.
    pub memory: u64,
}

/// The non-secret settings that a layered root's activation applies, as the
/// [module](self#configuration) describes them.
///
/// [`check`](Self::check) checks that they keep every limit, and [`ActivationRecord::new`] admits
/// only a configuration that does.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Configuration {
    /// The image.
    pub image: Image,
    /// The upper.
    pub upper: Upper,
    /// The overlay's mode.
    pub overlay: OverlayMode,
    /// The export, if any.
    pub export: Option<Export>,
    /// The number of host mappings that the host sends once the guest is ready: 1 to
    /// [`MAX_MAPPINGS`] with an export, and 0 without one.
    pub mappings: usize,
    /// The network, for a VM that has one.
    pub network: Option<Network>,
    /// The workload's view.
    pub workload: WorkloadView,
    /// The resources that the guest must have.
    pub resources: ExpectedResources,
}

impl Configuration {
    /// Checks that the configuration keeps every limit of the [module](self#limits), and returns
    /// the first that it breaks, in the order of the configuration's fields.
    ///
    /// It refuses a GPT disk's first-partition bound of 0; an export without mappings, mappings
    /// without an export, and more than [`MAX_MAPPINGS`] mappings; a proxy endpoint that is empty
    /// or longer than [`MAX_PROXY_ENDPOINT_BYTES`], or holds a byte other than a visible ASCII
    /// character, or an `@`; a hostname that is empty or longer than [`MAX_HOSTNAME_BYTES`], or
    /// holds a NUL; a workload identity whose UID or GID is 0 or `u32::MAX`; and a CPU count or
    /// memory size of 0.
    pub fn check(&self) -> Result<(), ActivationError> {
        if let Image::Gpt {
            first_partition_bound: 0,
            ..
        } = self.image
        {
            return Err(ActivationError::FirstPartitionBound);
        }
        match (self.export.is_some(), self.mappings) {
            (true, 0) => return Err(ActivationError::ExportWithoutMappings),
            (true, mappings) if mappings > MAX_MAPPINGS => {
                return Err(ActivationError::TooManyMappings(mappings));
            }
            (false, 1..) => return Err(ActivationError::MappingsWithoutExport),
            _ => {}
        }
        if let Some(proxy) = self
            .network
            .as_ref()
            .and_then(|network| network.proxy.as_ref())
            && !is_proxy_endpoint(proxy)
        {
            return Err(ActivationError::ProxyEndpoint);
        }
        let hostname = &self.workload.hostname;
        if hostname.is_empty() || hostname.len() > MAX_HOSTNAME_BYTES || hostname.contains(&0) {
            return Err(ActivationError::Hostname);
        }
        // The kernel reads `u32::MAX` as an unchanged ID, which the guest's entry refuses.
        if let Some(WorkloadIdentity { uid, gid }) = self.workload.identity
            && [uid, gid].into_iter().any(|id| id == 0 || id == u32::MAX)
        {
            return Err(ActivationError::WorkloadIdentity);
        }
        if self.resources.cpus == 0 {
            return Err(ActivationError::Cpus);
        }
        if self.resources.memory == 0 {
            return Err(ActivationError::Memory);
        }
        Ok(())
    }
}

/// An activation record: the [`VERSION`] of its schema, the [`RestoreSection`] of a clone, and
/// the [`Configuration`], which keeps every limit of the [module](self#limits).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ActivationRecord {
    /// The restore section, for a clone.
    restore: Option<RestoreSection>,
    /// The configuration, which keeps every limit.
    configuration: Configuration,
}

impl ActivationRecord {
    /// Returns the record of `configuration`, with `restore`, the restore section of a clone, or
    /// without one for a cold boot.
    ///
    /// Fails with the first limit that the configuration breaks, as [`Configuration::check`]
    /// finds it.
    pub fn new(
        restore: Option<RestoreSection>,
        configuration: Configuration,
    ) -> Result<Self, ActivationError> {
        configuration.check()?;
        Ok(Self {
            restore,
            configuration,
        })
    }

    /// Returns the version of the record's schema: [`VERSION`].
    pub fn version(&self) -> u16 {
        VERSION
    }

    /// Returns the restore section of a clone's record, or `None` for a cold boot's.
    pub fn restore(&self) -> Option<&RestoreSection> {
        self.restore.as_ref()
    }

    /// Returns the configuration.
    pub fn configuration(&self) -> &Configuration {
        &self.configuration
    }
}

/// What Ready reports about a layered root's activation: the identity of the configuration that
/// the activation applied, and, for a clone, the transaction ID of the restore that it activated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReadyActivation {
    /// The identity of the configuration that the activation applied.
    pub identity: ConfigurationIdentity,
    /// The transaction ID of a clone's restore, or `None` for a cold boot.
    pub transaction: Option<TransactionId>,
}

/// Why an activation record was refused: the limit or the rule that it breaks.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ActivationError {
    /// A GPT disk's first-partition bound is 0.
    FirstPartitionBound,
    /// The configuration has an export, but no mappings.
    ExportWithoutMappings,
    /// The configuration has mappings, but no export.
    MappingsWithoutExport,
    /// The configuration announces more than [`MAX_MAPPINGS`] mappings, this many.
    TooManyMappings(usize),
    /// The proxy endpoint is empty, longer than [`MAX_PROXY_ENDPOINT_BYTES`], or holds a byte
    /// other than a visible ASCII character, or an `@`.
    ProxyEndpoint,
    /// The hostname is empty, longer than [`MAX_HOSTNAME_BYTES`], or holds a NUL.
    Hostname,
    /// The workload identity's UID or GID is 0, or `u32::MAX`, which the kernel reads as an
    /// unchanged ID.
    WorkloadIdentity,
    /// The expected CPU count is 0.
    Cpus,
    /// The expected memory size is 0.
    Memory,
}

impl fmt::Display for ActivationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FirstPartitionBound => {
                formatter.write_str("the bound of a GPT disk's first partition is 0")
            }
            Self::ExportWithoutMappings => formatter.write_str("the export has no mappings"),
            Self::MappingsWithoutExport => formatter.write_str("the mappings have no export"),
            Self::TooManyMappings(mappings) => write!(
                formatter,
                "{mappings} mappings are more than the {MAX_MAPPINGS} that a record announces"
            ),
            Self::ProxyEndpoint => write!(
                formatter,
                "the proxy endpoint is not 1 to {MAX_PROXY_ENDPOINT_BYTES} visible ASCII \
                 characters without an @"
            ),
            Self::Hostname => write!(
                formatter,
                "the hostname is not 1 to {MAX_HOSTNAME_BYTES} bytes without a NUL"
            ),
            Self::WorkloadIdentity => {
                formatter.write_str("the workload identity's UID or GID is 0 or 4294967295")
            }
            Self::Cpus => formatter.write_str("the expected CPU count is 0"),
            Self::Memory => formatter.write_str("the expected memory size is 0"),
        }
    }
}

impl std::error::Error for ActivationError {}

/// Returns whether `endpoint` is a proxy endpoint that a record holds: 1 to
/// [`MAX_PROXY_ENDPOINT_BYTES`] visible ASCII characters, without an `@`, which would start a
/// URL's user information, where credentials go.
fn is_proxy_endpoint(endpoint: &str) -> bool {
    !endpoint.is_empty()
        && endpoint.len() <= MAX_PROXY_ENDPOINT_BYTES
        && endpoint
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'@')
}

/// Writes `bytes` as lowercase hexadecimal digits, two for each byte.
fn write_hex(formatter: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
    bytes
        .iter()
        .try_for_each(|byte| write!(formatter, "{byte:02x}"))
}
