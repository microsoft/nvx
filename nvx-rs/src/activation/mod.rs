//! The activation record of a layered root: one bounded, versioned, non-secret record of the
//! settings that a root's activation applies, and what Ready reports about that activation, which
//! the host, the guest, and every adapter's framing share.
//!
//! A layered root's guest assembles its root and enters its workload with settings that the host
//! chooses for each launch. A clone that the host restores from a template keeps the template's
//! captured configuration, so its record gives it the settings of its own launch: everything that
//! it needs to activate. A cold boot's record holds the settings that its guest applied, in the
//! same form, and every layered root reports its activation in Ready in the same way:
//!
//! - [`ActivationRecord`]: the [`VERSION`] of its schema, the [`RestoreSection`] of a clone, and
//!   the [`Configuration`]. A clone's restore section holds the restore's [`TransactionId`], 16
//!   random bytes that the host picks for each restore and keeps across the restore's retries.
//!   A cold boot's record has no restore section.
//! - [`Configuration`]: the non-secret settings that the activation applies, which the
//!   [configuration](#configuration) lists.
//! - [`ReadyActivation`]: what Ready reports about the activation: that it applied, which every
//!   layered root reports, and the transaction ID, which only a clone reports.
//!
//! The [guest-agent guidelines] give the order in which an init completes a restore before Ready:
//! it applies the record, with this restore's own network policy, mounts the runtime's file
//! systems, starts the announcer when the record publishes the workload's listeners, and switches
//! into the root once.
//!
//! [guest-agent guidelines]: https://github.com/microsoft/nvx/blob/dev/doc/guest-agent-guidelines.md
//!
//! [`ActivationRecord::new`] admits a configuration only once [`Configuration::check`] finds that
//! it keeps every [limit](#limits), so that every record holds settings that a guest can apply.
//! [`ActivationRecord::encode`] writes a record's canonical [encoding](#encoding), and
//! [`ActivationRecord::decode`] reads it back, by the same rules, and [`ReadyActivation`] has a
//! canonical [encoding](#ready) of its own. The module depends on nothing outside the standard
//! library and the crate's `protocol` and `uuid` modules, so it builds without features, and the
//! host, the guest, and every adapter's framing share it.
//!
//! # Configuration
//!
//! A [`Configuration`] holds:
//!
//! - the [`Image`]: its format, an EROFS filesystem or a GPT disk, and the EROFS filesystem's
//!   UUID or the GPT disk's GUID that the guest expects, if any;
//! - the [`Upper`]: a tmpfs with its cap in percent of the VM's memory, or the scratch with its
//!   journal policy;
//! - the [`Export`], if any: one export, a directory or an aggregate, and the number of host
//!   mappings that the host sends once the guest is ready, from 1 to [`MAX_MAPPINGS`]. A
//!   configuration without an export has no mappings;
//! - the [`Network`] of a VM that has one: the address of its DNS server, if any, the
//!   [`Publication`] of the workload's listeners, and the endpoint of its proxy, if any, which
//!   holds no credentials;
//! - the [`View`]: the hostname and the [`MachineIdMode`], the image's own machine ID or one that
//!   the guest generates. Each execution names its own identity. A cold boot's view is the same
//!   [`View`], which [`protocol::view`](crate::protocol::view) defines for both;
//! - the root's settings: the [`OverlayMode`], the [`Journal`] policy of a scratch upper, which
//!   [`protocol::root`](crate::protocol::root) defines for cold boots too, and the bound of a GPT
//!   disk's first partition;
//! - the [`ExpectedResources`]: the CPU count and the memory size that the guest must have, as a
//!   clone's repair produces them.
//!
//! [`MachineIdMode`]: crate::protocol::view::MachineIdMode
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
//! The limits are public constants: a record's encoding holds at most [`MAX_RECORD_BYTES`] bytes,
//! so that a restore packet could carry the same bytes, a configuration announces at most
//! [`MAX_MAPPINGS`] mappings, a hostname holds at most [`MAX_HOSTNAME_BYTES`] bytes, and a proxy
//! endpoint at most [`MAX_PROXY_ENDPOINT_BYTES`]. The limits of a record's parts keep every
//! record's encoding within [`MAX_RECORD_BYTES`].
//!
//! [`Configuration::check`] also refuses an export without mappings, mappings without an export,
//! a tmpfs cap outside 1 to 90 percent, a hostname with a NUL, a proxy endpoint with anything
//! other than visible ASCII characters or with an `@`, which would start credentials, and a
//! zero CPU count, memory size, or bound of a GPT disk's first partition.
//!
//! # Encoding
//!
//! A record's canonical encoding is the record's [`VERSION`], a little-endian `u16`, then its
//! fields, without padding, in the increasing order of their tags, each at most once. A field is
//! its tag, one byte, the number of bytes of its value, one byte, and its value:
//!
//! | Tag | [`Field`] | Value | Bytes | Present |
//! | --- | --- | --- | --- | --- |
//! | `0x01` | [`Restore`](Field::Restore) | The transaction ID | 16 | In a clone's record |
//! | `0x10` | [`Image`](Field::Image) | The format, `1` for EROFS or `2` for GPT, and a GPT disk's first-partition bound, a `u64` | 1 for EROFS, 9 for GPT | Always |
//! | `0x11` | [`ImageIdentity`](Field::ImageIdentity) | The UUID or the disk GUID that the guest expects, in the order of its canonical text | 16 | If the guest expects one |
//! | `0x20` | [`Upper`](Field::Upper) | `1` for a tmpfs and its cap, 1 to 90 percent, or `2` for the scratch and its journal policy, `1` allowed or `2` forbidden | 2 | Always |
//! | `0x30` | [`Overlay`](Field::Overlay) | `1` for trusted lowers, or `2` for untrusted lowers | 1 | Always |
//! | `0x40` | [`Export`](Field::Export) | The shape, `1` for a directory or `2` for an aggregate | 1 | With an export |
//! | `0x41` | [`Mappings`](Field::Mappings) | The number of mappings, a `u16` from 1 to [`MAX_MAPPINGS`] | 2 | With an export |
//! | `0x50` | [`Network`](Field::Network) | `1` if listeners are published, or `2` if not | 1 | With a network |
//! | `0x51` | [`Dns`](Field::Dns) | The DNS server's IPv4 or IPv6 address | 4 or 16 | With a network that has one |
//! | `0x52` | [`Proxy`](Field::Proxy) | The proxy endpoint | 1 to [`MAX_PROXY_ENDPOINT_BYTES`] | With a network that has one |
//! | `0x60` | [`Hostname`](Field::Hostname) | The hostname | 1 to [`MAX_HOSTNAME_BYTES`] | Always |
//! | `0x61` | [`MachineId`](Field::MachineId) | `1` for the image's machine ID, or `2` for a generated one | 1 | Always |
//! | `0x70` | [`Resources`](Field::Resources) | The CPU count, a `u32`, and the memory size, a `u64` | 12 | Always |
//!
//! Integers are little-endian, and identities, IDs, and addresses are their bytes, in order. The
//! largest record, with every field at its largest, holds 424 bytes.
//!
//! [`ActivationRecord::decode`] refuses bytes that are longer than [`MAX_RECORD_BYTES`], end
//! inside the version or a field, or hold another version; a field with an unknown tag, a
//! repeated field, and a field out of order; a value with another number of bytes than its
//! contents take, or with a code that the version does not define; a mapping count of 0, since
//! a record without mappings leaves the field out; a record without a field that is always
//! present, or with a DNS server or a proxy but
//! no network; and every configuration that [`Configuration::check`] refuses, such as one with
//! an export without mappings, or with mappings without an export. A record therefore has
//! exactly one encoding, which [`ActivationRecord::encode`] writes, and which the decoder reads
//! back to the same record.
//!
//! # Ready
//!
//! A guest reports Ready once its activation has applied its record, and
//! [`ReadyActivation::of`] returns what that Ready reports about the activation: a clone's
//! transaction ID, and nothing more for a cold boot. The transaction ID ties a clone's Ready to
//! the record of its restore, since the host picks a new transaction ID for each restore and
//! sends no other record under it. What Ready reports has a canonical encoding, which
//! [`ReadyActivation::encode`] writes and [`ReadyActivation::decode`] reads back: `0` for a cold
//! boot, or `1` and the clone's 16-byte transaction ID, [`ReadyActivation::COLD_LEN`] or
//! [`ReadyActivation::CLONE_LEN`] bytes. A Ready that embeds these bytes therefore has the same
//! encoding for a clone as for a cold boot.
//!
//! The conformance vectors of `tests/vectors/activation-v2.txt` hold records, and what Ready
//! reports about their activations, in both encodings, and bytes that every decoder refuses.
//!
//! # Example
//!
//! ```
//! use nvx::activation::{
//!     ActivationRecord, Configuration, ExpectedResources, Image, RestoreSection, TransactionId,
//!     Upper,
//! };
//! use nvx::protocol::root::OverlayMode;
//! use nvx::protocol::view::{MachineIdMode, View};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let configuration = Configuration {
//!     image: Image::Erofs {
//!         uuid: Some("0123abcd-4567-89ef-0123-456789abcdef".parse()?),
//!     },
//!     upper: Upper::Tmpfs { max_memory_percent: 50 },
//!     overlay: OverlayMode::UntrustedLowers,
//!     export: None,
//!     mappings: 0,
//!     network: None,
//!     view: View {
//!         hostname: b"sandbox".to_vec(),
//!         machine_id: MachineIdMode::Generated,
//!     },
//!     resources: ExpectedResources { cpus: 2, memory: 1 << 30 },
//! };
//! // A cold boot's record has no restore section.
//! let cold = ActivationRecord::new(None, configuration.clone())?;
//! assert!(cold.restore().is_none());
//! // A clone's names only its restore's transaction.
//! let restore = RestoreSection {
//!     transaction: TransactionId::from_bytes([0x22; 16]),
//! };
//! let clone = ActivationRecord::new(Some(restore), configuration)?;
//! assert_eq!(clone.restore(), Some(&restore));
//! // Its canonical encoding reads back to it.
//! let bytes = clone.encode();
//! assert_eq!(ActivationRecord::decode(&bytes)?, clone);
//! # Ok(())
//! # }
//! ```

use std::fmt;
use std::net::IpAddr;

use crate::protocol::MAX_HOST_MAPPINGS;
use crate::protocol::root::{ImageFormat, Journal, OverlayMode};
use crate::protocol::view::{MAX_HOSTNAME_BYTES, View, is_hostname};
use crate::uuid::Uuid;

mod codec;
mod ready;
#[cfg(test)]
mod tests;

pub use self::codec::Field;

/// The version of the activation contract that this build speaks: the schema of an
/// [`ActivationRecord`], which every record names. A record of another version is refused.
pub const VERSION: u16 = 2;

/// The most bytes of a record's encoding, so that a restore packet could carry the same bytes.
pub const MAX_RECORD_BYTES: usize = 1024;

/// The most host mappings that a configuration announces: as many as a host mapping table holds,
/// [`MAX_HOST_MAPPINGS`].
pub const MAX_MAPPINGS: usize = MAX_HOST_MAPPINGS;

/// The longest proxy endpoint, in bytes.
pub const MAX_PROXY_ENDPOINT_BYTES: usize = 255;

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

/// The restore section of a clone's record: only the restore's transaction ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RestoreSection {
    /// The restore's transaction ID, which the clone's Ready reports.
    pub transaction: TransactionId,
}

/// The image of a layered root, on its `distro` block device, in the format that the record
/// names: the guest never probes the image for another.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Image {
    /// An EROFS filesystem, the overlay's one lower layer.
    Erofs {
        /// The UUID that the filesystem's superblock must hold, if the guest expects one.
        uuid: Option<Uuid>,
    },
    /// A GPT disk, whose first partition holds opaque bytes, and whose later partitions hold the
    /// overlay's lower layers.
    Gpt {
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
    Tmpfs {
        /// The most that the upper may hold, in percent of the VM's memory: 1 to 90.
        max_memory_percent: u8,
    },
    /// A whole-device ext4 filesystem on the `scratch` block device.
    Scratch {
        /// Whether the scratch's filesystem may have a journal.
        journal: Journal,
    },
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
    /// The address of the DNS server that the guest's resolver configuration names, if any.
    pub dns: Option<IpAddr>,
    /// Whether the workload's listeners are published.
    pub publication: Publication,
    /// The endpoint of the proxy through which the workload reaches the network, as the guest
    /// sees it, such as `http://10.0.0.1:3128`, if any: 1 to [`MAX_PROXY_ENDPOINT_BYTES`] visible
    /// ASCII characters, without an `@`, so that it holds no credentials.
    pub proxy: Option<String>,
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
    pub view: View,
    /// The resources that the guest must have.
    pub resources: ExpectedResources,
}

impl Configuration {
    /// Checks that the configuration keeps every limit of the [module](self#limits), and returns
    /// the first that it breaks, in the order of the configuration's fields.
    ///
    /// It refuses a GPT disk's first-partition bound of 0; a tmpfs cap outside 1 to 90 percent;
    /// an export without mappings, mappings
    /// without an export, and more than [`MAX_MAPPINGS`] mappings; a proxy endpoint that is empty
    /// or longer than [`MAX_PROXY_ENDPOINT_BYTES`], or holds a byte other than a visible ASCII
    /// character, or an `@`; a hostname that is empty or longer than [`MAX_HOSTNAME_BYTES`], or
    /// holds a NUL; and a CPU count or memory size of 0.
    pub fn check(&self) -> Result<(), ActivationError> {
        if let Image::Gpt {
            first_partition_bound: 0,
            ..
        } = self.image
        {
            return Err(ActivationError::FirstPartitionBound);
        }
        if let Upper::Tmpfs { max_memory_percent } = self.upper
            && !(1..=90).contains(&max_memory_percent)
        {
            return Err(ActivationError::TmpfsMemoryPercent(max_memory_percent));
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
        if !is_hostname(&self.view.hostname) {
            return Err(ActivationError::Hostname);
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

/// What Ready reports about a layered root's activation: that it applied, and, for a clone, the
/// transaction ID of the restore that it activated. Its canonical [encoding](self#ready) holds
/// [`COLD_LEN`](Self::COLD_LEN) or [`CLONE_LEN`](Self::CLONE_LEN) bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReadyActivation {
    /// The transaction ID of a clone's restore, or `None` for a cold boot.
    pub transaction: Option<TransactionId>,
}

/// Why an activation record, or what Ready reports about an activation, was refused: the limit or
/// the rule that it breaks.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ActivationError {
    /// A GPT disk's first-partition bound is 0.
    FirstPartitionBound,
    /// The tmpfs cap is outside 1 to 90 percent: this value.
    TmpfsMemoryPercent(u8),
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
    /// The expected CPU count is 0.
    Cpus,
    /// The expected memory size is 0.
    Memory,
    /// The bytes hold more than [`MAX_RECORD_BYTES`]: this many.
    TooLong(usize),
    /// The bytes end inside the record's version or inside one of its fields.
    Truncated,
    /// The record's version is not [`VERSION`], but this one.
    Version(u16),
    /// A field has a tag that the record's version does not define: this one.
    UnknownField(u8),
    /// A field is repeated.
    DuplicateField(Field),
    /// A field follows a field whose tag is greater.
    FieldOrder(Field),
    /// A field's value holds another number of bytes than its contents take: this many.
    FieldLength(Field, usize),
    /// A field's value holds a code that the record's version does not define, or a form that
    /// no record takes, such as a mapping count of 0.
    FieldValue(Field),
    /// A field that every record holds is missing.
    MissingField(Field),
    /// A field that only a record with a network holds, its DNS server's or its proxy's, is
    /// present without one.
    WithoutNetwork(Field),
    /// The bytes of what Ready reports are neither `0`, nor `1` followed by a transaction ID.
    Ready,
}

impl fmt::Display for ActivationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FirstPartitionBound => {
                formatter.write_str("the bound of a GPT disk's first partition is 0")
            }
            Self::TmpfsMemoryPercent(percent) => write!(
                formatter,
                "the tmpfs cap of {percent}% of the VM's memory is not 1 to 90%"
            ),
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
            Self::Cpus => formatter.write_str("the expected CPU count is 0"),
            Self::Memory => formatter.write_str("the expected memory size is 0"),
            Self::TooLong(length) => write!(
                formatter,
                "the record's {length} bytes are more than the {MAX_RECORD_BYTES} that a record \
                 holds"
            ),
            Self::Truncated => {
                formatter.write_str("the record ends inside its version or one of its fields")
            }
            Self::Version(version) => {
                write!(
                    formatter,
                    "the record's version is {version}, not {VERSION}"
                )
            }
            Self::UnknownField(tag) => {
                write!(
                    formatter,
                    "the record holds a field of the unknown tag {tag:#04x}"
                )
            }
            Self::DuplicateField(field) => {
                write!(formatter, "the record repeats its {field} field")
            }
            Self::FieldOrder(field) => {
                write!(formatter, "the record's {field} field is out of order")
            }
            Self::FieldLength(field, length) => write!(
                formatter,
                "the record's {field} field holds {length} bytes, which its value never takes"
            ),
            Self::FieldValue(field) => write!(
                formatter,
                "the record's {field} field holds a value that no record takes"
            ),
            Self::MissingField(field) => write!(formatter, "the record has no {field} field"),
            Self::WithoutNetwork(field) => {
                write!(formatter, "the record has a {field} field but no network")
            }
            Self::Ready => formatter
                .write_str("what Ready reports is neither 0, nor 1 followed by a transaction ID"),
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
