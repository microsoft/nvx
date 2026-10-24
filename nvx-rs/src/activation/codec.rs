//! The canonical encoding of an activation record, which the [module](super#encoding) describes:
//! [`ActivationRecord::encode`] writes it, and [`ActivationRecord::decode`] reads it back.

use std::fmt;
use std::net::IpAddr;
use std::num::NonZeroU64;

use super::{
    ActivationError, ActivationRecord, Configuration, ExpectedResources, Export, ExportKind,
    Identity, Image, Journal, MAX_RECORD_BYTES, MachineIdMode, Network, OverlayMode, Publication,
    RestoreSection, StoreGeneration, TemplateReference, TransactionId, Upper, VERSION,
    WorkloadIdentity, WorkloadView,
};
use crate::protocol::root::ImageFormat;
use crate::uuid::Uuid;

#[cfg(test)]
mod tests;

/// The bytes of a record's version.
const VERSION_LEN: usize = 2;
/// The bytes of a field's header: its tag, then the length of its value.
const FIELD_HEADER_LEN: usize = 2;
/// The code of an upper that is a tmpfs.
const TMPFS: u8 = 1;
/// The code of an upper that is the scratch.
const SCRATCH: u8 = 2;

/// A field of a record's encoding, which a refusal names: the fields in the order of their tags,
/// as the [module](super#encoding) lists them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Field {
    /// `0x01`: the restore section of a clone.
    Restore,
    /// `0x10`: the image's format, the identity of its binding, and a GPT disk's first-partition
    /// bound.
    Image,
    /// `0x11`: the UUID or the disk GUID that the guest expects of the image.
    ImageIdentity,
    /// `0x20`: the upper, and a scratch's binding and journal policy.
    Upper,
    /// `0x30`: the overlay's mode.
    Overlay,
    /// `0x40`: the export's shape, and the identity of its plan.
    Export,
    /// `0x41`: the number of host mappings.
    Mappings,
    /// `0x50`: the identity of the network's policy, and the publication of the listeners.
    Network,
    /// `0x51`: the address of the network's DNS server.
    Dns,
    /// `0x52`: the network's proxy endpoint.
    Proxy,
    /// `0x60`: the hostname.
    Hostname,
    /// `0x61`: the machine ID's mode.
    MachineId,
    /// `0x62`: the workload identity.
    WorkloadIdentity,
    /// `0x70`: the expected resources.
    Resources,
}

impl Field {
    /// Every field, in the order of their tags.
    pub const ALL: [Self; 14] = [
        Self::Restore,
        Self::Image,
        Self::ImageIdentity,
        Self::Upper,
        Self::Overlay,
        Self::Export,
        Self::Mappings,
        Self::Network,
        Self::Dns,
        Self::Proxy,
        Self::Hostname,
        Self::MachineId,
        Self::WorkloadIdentity,
        Self::Resources,
    ];

    /// Returns the field's tag.
    pub const fn tag(self) -> u8 {
        match self {
            Self::Restore => 0x01,
            Self::Image => 0x10,
            Self::ImageIdentity => 0x11,
            Self::Upper => 0x20,
            Self::Overlay => 0x30,
            Self::Export => 0x40,
            Self::Mappings => 0x41,
            Self::Network => 0x50,
            Self::Dns => 0x51,
            Self::Proxy => 0x52,
            Self::Hostname => 0x60,
            Self::MachineId => 0x61,
            Self::WorkloadIdentity => 0x62,
            Self::Resources => 0x70,
        }
    }

    /// Returns the field whose tag is `tag`, if any.
    pub fn from_tag(tag: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|field| field.tag() == tag)
    }
}

impl fmt::Display for Field {
    /// Writes the field's name, as a refusal names it.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Restore => "restore",
            Self::Image => "image",
            Self::ImageIdentity => "image identity",
            Self::Upper => "upper",
            Self::Overlay => "overlay",
            Self::Export => "export",
            Self::Mappings => "mappings",
            Self::Network => "network",
            Self::Dns => "DNS",
            Self::Proxy => "proxy",
            Self::Hostname => "hostname",
            Self::MachineId => "machine ID",
            Self::WorkloadIdentity => "workload identity",
            Self::Resources => "resources",
        })
    }
}

/// A choice that a field names with a one-byte code: a field's choices have the codes 1, 2, and
/// so on, as the [module](super#encoding) lists them, so that a zero byte names none.
trait Code: Sized {
    /// Returns the choice's code.
    fn code(self) -> u8;
    /// Returns the choice whose code is `code`, if any.
    fn from_code(code: u8) -> Option<Self>;
}

/// Gives each choice of an enum its code, both ways, with matches that name every choice.
macro_rules! codes {
    ($type:ty { $($choice:path => $code:literal),+ $(,)? }) => {
        impl Code for $type {
            fn code(self) -> u8 {
                match self {
                    $($choice => $code,)+
                }
            }

            fn from_code(code: u8) -> Option<Self> {
                match code {
                    $($code => Some($choice),)+
                    _ => None,
                }
            }
        }
    };
}

codes!(ImageFormat {
    ImageFormat::Erofs => 1,
    ImageFormat::Gpt => 2,
});
codes!(Journal {
    Journal::Allowed => 1,
    Journal::Forbidden => 2,
});
codes!(OverlayMode {
    OverlayMode::TrustedLowers => 1,
    OverlayMode::UntrustedLowers => 2,
});
codes!(ExportKind {
    ExportKind::Directory => 1,
    ExportKind::Aggregate => 2,
});
codes!(Publication {
    Publication::Published => 1,
    Publication::Unpublished => 2,
});
codes!(MachineIdMode {
    MachineIdMode::Image => 1,
    MachineIdMode::Generated => 2,
});

impl ActivationRecord {
    /// Returns the record's canonical encoding, as the [module](super#encoding) describes it: at
    /// most [`MAX_RECORD_BYTES`] bytes, which [`decode`](Self::decode) reads back to this record.
    ///
    /// It never fails: a record keeps every limit, as [`ActivationRecord::new`] and
    /// [`decode`](Self::decode) check them, so the encoder never writes what the decoder refuses.
    pub fn encode(&self) -> Vec<u8> {
        let configuration = &self.configuration;
        let mut encoder = Encoder::new();
        if let Some(RestoreSection {
            transaction,
            template,
        }) = &self.restore
        {
            encoder.field(
                Field::Restore,
                &[
                    transaction.as_bytes(),
                    &template.generation.get().get().to_le_bytes(),
                    template.key.as_bytes(),
                ],
            );
        }
        let image = &configuration.image;
        let format = [image.format().code()];
        match image {
            Image::Erofs { binding, .. } => {
                encoder.field(Field::Image, &[&format, binding.as_bytes()]);
            }
            Image::Gpt {
                binding,
                first_partition_bound,
                ..
            } => encoder.field(
                Field::Image,
                &[
                    &format,
                    binding.as_bytes(),
                    &first_partition_bound.to_le_bytes(),
                ],
            ),
        }
        if let Some(expected) = image.expected() {
            encoder.field(Field::ImageIdentity, &[expected.as_bytes()]);
        }
        match configuration.upper {
            Upper::Tmpfs => encoder.field(Field::Upper, &[&[TMPFS]]),
            Upper::Scratch { binding, journal } => encoder.field(
                Field::Upper,
                &[&[SCRATCH], binding.as_bytes(), &[journal.code()]],
            ),
        }
        encoder.field(Field::Overlay, &[&[configuration.overlay.code()]]);
        if let Some(Export { kind, plan }) = configuration.export {
            encoder.field(Field::Export, &[&[kind.code()], plan.as_bytes()]);
            // A record with an export announces 1 to `MAX_MAPPINGS` mappings, which a `u16` holds.
            let mappings = configuration.mappings as u16;
            encoder.field(Field::Mappings, &[&mappings.to_le_bytes()]);
        }
        if let Some(network) = &configuration.network {
            encoder.field(
                Field::Network,
                &[network.policy.as_bytes(), &[network.publication.code()]],
            );
            match network.dns {
                Some(IpAddr::V4(address)) => encoder.field(Field::Dns, &[&address.octets()]),
                Some(IpAddr::V6(address)) => encoder.field(Field::Dns, &[&address.octets()]),
                None => {}
            }
            if let Some(proxy) = &network.proxy {
                encoder.field(Field::Proxy, &[proxy.as_bytes()]);
            }
        }
        let workload = &configuration.workload;
        encoder.field(Field::Hostname, &[&workload.hostname]);
        encoder.field(Field::MachineId, &[&[workload.machine_id.code()]]);
        if let Some(WorkloadIdentity { uid, gid }) = workload.identity {
            encoder.field(
                Field::WorkloadIdentity,
                &[&uid.to_le_bytes(), &gid.to_le_bytes()],
            );
        }
        let ExpectedResources { cpus, memory } = configuration.resources;
        encoder.field(
            Field::Resources,
            &[&cpus.to_le_bytes(), &memory.to_le_bytes()],
        );
        encoder.bytes
    }

    /// Reads a record from its canonical encoding, as the [module](super#encoding) describes it.
    ///
    /// Fails with the first rule that the bytes break, in this order: bytes longer than
    /// [`MAX_RECORD_BYTES`]; bytes that end inside the version; another version than [`VERSION`];
    /// then, field by field, a field whose tag and length end past the bytes, a field with an
    /// unknown tag, a repeated field, a field out of order, a value that ends past the bytes, and a
    /// value with another length than its contents take, with a code that the version does not
    /// define, with a mapping count or a store generation of 0, or with a proxy endpoint that is
    /// not UTF-8; then a missing field that every record holds, in the order of the tags; then a
    /// DNS server or a proxy without a network; then the first limit that [`Configuration::check`]
    /// finds broken, such as an export without mappings, or mappings without an export. So it reads
    /// exactly the bytes that [`encode`](Self::encode) writes.
    pub fn decode(bytes: &[u8]) -> Result<Self, ActivationError> {
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(ActivationError::TooLong(bytes.len()));
        }
        let (version, mut rest) = bytes
            .split_first_chunk::<VERSION_LEN>()
            .ok_or(ActivationError::Truncated)?;
        let version = u16::from_le_bytes(*version);
        if version != VERSION {
            return Err(ActivationError::Version(version));
        }
        let mut draft = Draft::default();
        let mut seen = Vec::with_capacity(Field::ALL.len());
        while !rest.is_empty() {
            let (&[tag, length], tail) = rest
                .split_first_chunk::<FIELD_HEADER_LEN>()
                .ok_or(ActivationError::Truncated)?;
            let field = Field::from_tag(tag).ok_or(ActivationError::UnknownField(tag))?;
            if seen.contains(&field) {
                return Err(ActivationError::DuplicateField(field));
            }
            if seen.last().is_some_and(|last: &Field| last.tag() > tag) {
                return Err(ActivationError::FieldOrder(field));
            }
            let (value, tail) = tail
                .split_at_checked(usize::from(length))
                .ok_or(ActivationError::Truncated)?;
            draft.read(field, value)?;
            seen.push(field);
            rest = tail;
        }
        draft.into_record()
    }
}

/// Writes a record's encoding: its version, then the fields that the caller writes in the order
/// of their tags.
struct Encoder {
    /// The encoding so far.
    bytes: Vec<u8>,
}

impl Encoder {
    /// Returns an encoder that has written the version.
    fn new() -> Self {
        let mut bytes = Vec::with_capacity(MAX_RECORD_BYTES);
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        Self { bytes }
    }

    /// Writes `field`, whose value is `parts`, one after the other.
    fn field(&mut self, field: Field, parts: &[&[u8]]) {
        let length: usize = parts.iter().map(|part| part.len()).sum();
        debug_assert!(
            length <= usize::from(u8::MAX),
            "the {field} field is too long"
        );
        self.bytes.push(field.tag());
        // A field's value holds at most `MAX_PROXY_ENDPOINT_BYTES`, the most that a byte counts.
        self.bytes.push(length as u8);
        for part in parts {
            self.bytes.extend_from_slice(part);
        }
    }
}

/// What the fields of an encoding have given so far, each once.
#[derive(Default)]
struct Draft {
    /// The restore section.
    restore: Option<RestoreSection>,
    /// The image, without what the guest expects of it.
    image: Option<Image>,
    /// What the guest expects of the image.
    image_identity: Option<Uuid>,
    /// The upper.
    upper: Option<Upper>,
    /// The overlay's mode.
    overlay: Option<OverlayMode>,
    /// The export.
    export: Option<Export>,
    /// The number of mappings: 1 or more.
    mappings: Option<usize>,
    /// The identity of the network's policy, and the publication of the listeners.
    network: Option<(Identity, Publication)>,
    /// The address of the DNS server.
    dns: Option<IpAddr>,
    /// The proxy endpoint.
    proxy: Option<String>,
    /// The hostname.
    hostname: Option<Vec<u8>>,
    /// The machine ID's mode.
    machine_id: Option<MachineIdMode>,
    /// The workload identity.
    identity: Option<WorkloadIdentity>,
    /// The expected resources.
    resources: Option<ExpectedResources>,
}

impl Draft {
    /// Reads `field`'s value, `bytes`.
    fn read(&mut self, field: Field, bytes: &[u8]) -> Result<(), ActivationError> {
        let mut value = Value::new(field, bytes);
        match field {
            Field::Restore => {
                let transaction = TransactionId::from_bytes(value.take()?);
                // A store assigns no template the generation 0.
                let generation = NonZeroU64::new(u64::from_le_bytes(value.take()?))
                    .map(StoreGeneration::new)
                    .ok_or(ActivationError::FieldValue(field))?;
                let key = Identity::from_bytes(value.take()?);
                self.restore = Some(RestoreSection {
                    transaction,
                    template: TemplateReference { generation, key },
                });
            }
            Field::Image => {
                let format = value.code::<ImageFormat>()?;
                let binding = Identity::from_bytes(value.take()?);
                self.image = Some(match format {
                    ImageFormat::Erofs => Image::Erofs {
                        binding,
                        uuid: None,
                    },
                    ImageFormat::Gpt => Image::Gpt {
                        binding,
                        disk_guid: None,
                        first_partition_bound: u64::from_le_bytes(value.take()?),
                    },
                });
            }
            Field::ImageIdentity => self.image_identity = Some(Uuid::from_bytes(value.take()?)),
            Field::Upper => {
                self.upper = Some(match value.take::<1>()? {
                    [TMPFS] => Upper::Tmpfs,
                    [SCRATCH] => Upper::Scratch {
                        binding: Identity::from_bytes(value.take()?),
                        journal: value.code()?,
                    },
                    _ => return Err(ActivationError::FieldValue(field)),
                });
            }
            Field::Overlay => self.overlay = Some(value.code()?),
            Field::Export => {
                let kind = value.code()?;
                let plan = Identity::from_bytes(value.take()?);
                self.export = Some(Export { kind, plan });
            }
            Field::Mappings => match u16::from_le_bytes(value.take()?) {
                // A record without mappings leaves the field out.
                0 => return Err(ActivationError::FieldValue(field)),
                mappings => self.mappings = Some(usize::from(mappings)),
            },
            Field::Network => {
                let policy = Identity::from_bytes(value.take()?);
                self.network = Some((policy, value.code()?));
            }
            Field::Dns => {
                // A DNS server's address is IPv4 or IPv6, which its length tells apart.
                self.dns = Some(if bytes.len() == 4 {
                    IpAddr::from(value.take::<4>()?)
                } else {
                    IpAddr::from(value.take::<16>()?)
                });
            }
            Field::Proxy => {
                let proxy = String::from_utf8(value.rest().to_vec())
                    .map_err(|_| ActivationError::ProxyEndpoint)?;
                self.proxy = Some(proxy);
            }
            Field::Hostname => self.hostname = Some(value.rest().to_vec()),
            Field::MachineId => self.machine_id = Some(value.code()?),
            Field::WorkloadIdentity => {
                let uid = u32::from_le_bytes(value.take()?);
                let gid = u32::from_le_bytes(value.take()?);
                self.identity = Some(WorkloadIdentity { uid, gid });
            }
            Field::Resources => {
                let cpus = u32::from_le_bytes(value.take()?);
                let memory = u64::from_le_bytes(value.take()?);
                self.resources = Some(ExpectedResources { cpus, memory });
            }
        }
        value.end()
    }

    /// Returns the record of the fields: the fields that every record holds must be there, a DNS
    /// server and a proxy only with a network, and the configuration must keep every limit.
    fn into_record(self) -> Result<ActivationRecord, ActivationError> {
        let missing = ActivationError::MissingField;
        let mut image = self.image.ok_or(missing(Field::Image))?;
        let upper = self.upper.ok_or(missing(Field::Upper))?;
        let overlay = self.overlay.ok_or(missing(Field::Overlay))?;
        let hostname = self.hostname.ok_or(missing(Field::Hostname))?;
        let machine_id = self.machine_id.ok_or(missing(Field::MachineId))?;
        let resources = self.resources.ok_or(missing(Field::Resources))?;
        if let Some(expected) = self.image_identity {
            match &mut image {
                Image::Erofs { uuid, .. } => *uuid = Some(expected),
                Image::Gpt { disk_guid, .. } => *disk_guid = Some(expected),
            }
        }
        let network = match self.network {
            Some((policy, publication)) => Some(Network {
                policy,
                dns: self.dns,
                publication,
                proxy: self.proxy,
            }),
            None if self.dns.is_some() => {
                return Err(ActivationError::WithoutNetwork(Field::Dns));
            }
            None if self.proxy.is_some() => {
                return Err(ActivationError::WithoutNetwork(Field::Proxy));
            }
            None => None,
        };
        let configuration = Configuration {
            image,
            upper,
            overlay,
            export: self.export,
            mappings: self.mappings.unwrap_or(0),
            network,
            workload: WorkloadView {
                hostname,
                machine_id,
                identity: self.identity,
            },
            resources,
        };
        ActivationRecord::new(self.restore, configuration)
    }
}

/// Reads the parts of a field's value in order, and refuses a value of another length than its
/// parts take.
struct Value<'a> {
    /// The field.
    field: Field,
    /// The value's length.
    length: usize,
    /// What remains of the value.
    rest: &'a [u8],
}

impl<'a> Value<'a> {
    /// Returns a reader of `field`'s value, `bytes`.
    fn new(field: Field, bytes: &'a [u8]) -> Self {
        Self {
            field,
            length: bytes.len(),
            rest: bytes,
        }
    }

    /// Returns the refusal of a value of another length than its parts take.
    fn length_error(&self) -> ActivationError {
        ActivationError::FieldLength(self.field, self.length)
    }

    /// Reads the next `N` bytes.
    fn take<const N: usize>(&mut self) -> Result<[u8; N], ActivationError> {
        let (part, rest) = self
            .rest
            .split_first_chunk::<N>()
            .ok_or_else(|| self.length_error())?;
        self.rest = rest;
        Ok(*part)
    }

    /// Reads the next byte, the code of a choice.
    fn code<T: Code>(&mut self) -> Result<T, ActivationError> {
        let [code] = self.take()?;
        T::from_code(code).ok_or(ActivationError::FieldValue(self.field))
    }

    /// Returns what remains of the value, which the caller reads whole.
    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.rest)
    }

    /// Checks that nothing remains of the value.
    fn end(self) -> Result<(), ActivationError> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(self.length_error())
        }
    }
}
