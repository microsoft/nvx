//! The compatibility key of a template, [`TemplateKey`], and the [`CaptureClass`] of the capture
//! that makes one.

use std::collections::BTreeMap;
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

use sha2::{Digest, Sha256};

use super::canonical::{CanonicalError, Reader, Writer};
use super::manifest::{
    AddressSpace, BlockGeometry, CPUID_ENTRY_LEN, CpuProfile, Device, DeviceRange,
    FILESYSTEM_SLOT_VERSION, Interrupts, Ipv6Identity, MAX_COMMAND_LINE_LEN, MAX_CPUID_ENTRIES,
    MAX_DEVICE_RANGES, MAX_DEVICE_WORDS, MAX_DEVICES, MAX_MEMORY_RANGES, MAX_PROCESSORS,
    MAX_TEXT_LEN, Machine, Memory, MemoryRange, Nic, PROCESSOR_COUNTS, PolicyIdentity, Rates,
    TemplateManifest, Topology, parse_profile_id,
};
use crate::activation;
use crate::contract;
use crate::error::{Error, Result};
use crate::host::platform::parse_hex32;
use crate::host::sandbox::SandboxNetwork;
use crate::host::sandbox::mount::MOUNT_TAG;
use crate::host::store::{ArtifactIdentity, MAX_IDENTITY_LEN, RuntimeIdentity};
use crate::hypervisor::Hypervisor;

/// The domain of a key's identity: the bytes that precede its canonical encoding in the digest.
const KEY_DOMAIN: &[u8] = b"nvx template key\0";

/// The domain of a capture class's identity: the bytes that precede its canonical encoding in the
/// digest.
const CLASS_DOMAIN: &[u8] = b"nvx capture class\0";

/// The generation of a CPU profile that a host derives, rather than a built-in one.
const HOST_GENERATION: &str = "host";

/// The versions of the contracts of a template's clones that its runtime identity does not
/// record: the snapshot format, the saved device state, the time ABI, the restore packet, and the
/// activation record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Contracts {
    /// [`contract::SNAPSHOT_FORMAT_VERSION`].
    snapshot: u64,
    /// [`contract::DEVICE_STATE_SCHEMA_VERSION`].
    device_state: u64,
    /// [`contract::TIME_ABI_VERSION`].
    time: u64,
    /// [`contract::RESTORE_PACKET_VERSION`].
    restore_packet: u64,
    /// [`activation::VERSION`], as a `u64`, as the other versions are.
    activation: u64,
}

impl Contracts {
    /// The versions that this crate speaks.
    const CURRENT: Self = Self {
        snapshot: contract::SNAPSHOT_FORMAT_VERSION,
        device_state: contract::DEVICE_STATE_SCHEMA_VERSION,
        time: contract::TIME_ABI_VERSION,
        restore_packet: contract::RESTORE_PACKET_VERSION,
        // `u64::from` is not `const`, and widening a `u16` is lossless.
        activation: activation::VERSION as u64,
    };

    /// Writes the versions.
    fn write(&self, writer: &mut Writer) {
        for version in [
            self.snapshot,
            self.device_state,
            self.time,
            self.restore_packet,
            self.activation,
        ] {
            writer.u64(version);
        }
    }

    /// Reads the versions.
    fn read(reader: &mut Reader<'_>) -> Result<Self, CanonicalError> {
        Ok(Self {
            snapshot: reader.u64("the snapshot format version")?,
            device_state: reader.u64("the device-state schema version")?,
            time: reader.u64("the time ABI version")?,
            restore_packet: reader.u64("the restore packet version")?,
            activation: reader.u64("the activation contract version")?,
        })
    }
}

/// Writes the runtime identity `runtime`: its artifacts, then its boot and control versions.
fn write_runtime(writer: &mut Writer, runtime: &RuntimeIdentity) {
    for artifact in [&runtime.openvmm, &runtime.kernel, &runtime.initramfs] {
        writer.text(artifact.as_str());
    }
    writer.u64(runtime.boot_contract);
    writer.text(&runtime.control_contract);
    writer.u64(runtime.control_session);
}

/// Reads an artifact identity, which `what` names.
fn read_artifact(reader: &mut Reader<'_>, what: &str) -> Result<ArtifactIdentity, CanonicalError> {
    let text = reader.text(MAX_IDENTITY_LEN, what)?;
    ArtifactIdentity::new(text).map_err(|error| CanonicalError::new(error.message()))
}

/// Reads a runtime identity, as [`write_runtime`] writes it.
fn read_runtime(reader: &mut Reader<'_>) -> Result<RuntimeIdentity, CanonicalError> {
    let runtime = RuntimeIdentity {
        openvmm: read_artifact(reader, "the OpenVMM identity")?,
        kernel: read_artifact(reader, "the kernel identity")?,
        initramfs: read_artifact(reader, "the initramfs identity")?,
        boot_contract: reader.u64("the boot contract version")?,
        control_contract: reader
            .text(MAX_IDENTITY_LEN, "the control contract revision")?
            .to_owned(),
        control_session: reader.u64("the control session version")?,
    };
    runtime
        .check()
        .map_err(|error| CanonicalError::new(error.message()))?;
    Ok(runtime)
}

/// Writes a block geometry.
fn write_geometry(writer: &mut Writer, geometry: &BlockGeometry) {
    writer.u64(geometry.length);
    writer.u32(geometry.logical_block_size);
    writer.u32(geometry.physical_block_size);
}

/// Reads a block geometry, which `what` names.
fn read_geometry(reader: &mut Reader<'_>, what: &str) -> Result<BlockGeometry, CanonicalError> {
    Ok(BlockGeometry {
        length: reader.u64(what)?,
        logical_block_size: reader.u32(what)?,
        physical_block_size: reader.u32(what)?,
    })
}

/// Returns the SHA-256 digest of `bytes` after `domain`.
fn digest(domain: &[u8], bytes: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update(bytes);
    digest.finalize().into()
}

/// Writes `bytes` as lowercase hexadecimal digits.
fn write_hex(formatter: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
    bytes
        .iter()
        .try_for_each(|byte| write!(formatter, "{byte:02x}"))
}

/// Declares a 32-byte identity: the domain-separated SHA-256 digest of a canonical encoding.
macro_rules! identity {
    ($(#[$attribute:meta])* $name:ident, $what:literal) => {
        $(#[$attribute])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name([u8; 32]);

        impl $name {
            /// Returns the identity whose bytes are `bytes`.
            pub const fn from_bytes(bytes: [u8; 32]) -> Self {
                Self(bytes)
            }

            /// Returns the bytes of the identity.
            pub const fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write_hex(formatter, &self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, concat!(stringify!($name), "({})"), self)
            }
        }

        impl std::str::FromStr for $name {
            type Err = Error;

            /// Parses the text form of the identity, 64 hexadecimal digits, failing with
            /// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for anything else.
            fn from_str(text: &str) -> Result<Self> {
                parse_hex32(text).map(Self).ok_or_else(|| {
                    Error::unsupported(format!(
                        concat!("{:?} is not ", $what, ", which is 64 hexadecimal digits"),
                        text
                    ))
                })
            }
        }
    };
}

identity!(
    /// The identity of a [`TemplateKey`]: the SHA-256 digest of its canonical encoding, after the
    /// domain `nvx template key` and a NUL, so that it names a key of one encoding version alone.
    ///
    /// Its text form, which `Display` writes and `FromStr` parses, is 64 lowercase hexadecimal
    /// digits; parsing also accepts uppercase ones. It converts to and from the
    /// [`activation::Identity`] by which a clone's activation record names the key of its
    /// template, in its [`TemplateReference`](activation::TemplateReference), without loss.
    KeyIdentity,
    "a template key's identity"
);

impl From<KeyIdentity> for activation::Identity {
    /// Returns the identity whose bytes are the key identity's.
    fn from(identity: KeyIdentity) -> Self {
        Self::from_bytes(identity.0)
    }
}

impl From<activation::Identity> for KeyIdentity {
    /// Returns the key identity whose bytes are the identity's.
    fn from(identity: activation::Identity) -> Self {
        Self(*identity.as_bytes())
    }
}

identity!(
    /// The identity of a [`CaptureClass`]: the SHA-256 digest of its canonical encoding, after the
    /// domain `nvx capture class` and a NUL.
    ///
    /// Its text form, which `Display` writes and `FromStr` parses, is 64 lowercase hexadecimal
    /// digits; parsing also accepts uppercase ones.
    ClassIdentity,
    "a capture class's identity"
);

/// The invariant settings of a template that its key holds beyond its machine, such as the
/// isolation and resource settings of its guest, by name: what its adapter fixes for the
/// template and every clone of it, and no restore changes.
///
/// A name is 1 to [`MAX_NAME_LEN`](Self::MAX_NAME_LEN) bytes of lowercase ASCII letters, digits,
/// `.`, `-`, and `_`, which starts with a letter, and a value 0 to
/// [`MAX_VALUE_LEN`](Self::MAX_VALUE_LEN) printable ASCII characters, spaces included. A template
/// holds at most [`MAX_SETTINGS`](Self::MAX_SETTINGS) of them, in the order of their names.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Invariants(BTreeMap<String, String>);

impl Invariants {
    /// Most settings of a template.
    pub const MAX_SETTINGS: usize = 64;
    /// Longest name of a setting, in bytes.
    pub const MAX_NAME_LEN: usize = 64;
    /// Longest value of a setting, in bytes.
    pub const MAX_VALUE_LEN: usize = 256;

    /// Returns no settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the settings with `name` set to `value`, which replaces any earlier value of
    /// `name`.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a name or a value
    /// of another form than the [type](Self) allows, and for a new setting beyond
    /// [`MAX_SETTINGS`](Self::MAX_SETTINGS).
    pub fn with(mut self, name: &str, value: &str) -> Result<Self> {
        check_setting(name, value).map_err(|error| Error::unsupported(error.to_string()))?;
        if !self.0.contains_key(name) && self.0.len() == Self::MAX_SETTINGS {
            return Err(Error::unsupported(format!(
                "a template holds at most {} invariant settings",
                Self::MAX_SETTINGS
            )));
        }
        self.0.insert(name.to_owned(), value.to_owned());
        Ok(self)
    }

    /// Returns the value of the setting `name`, if the settings hold it.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    /// Returns the settings, in the order of their names.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    /// Returns the number of settings.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns whether there are no settings.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Writes the settings: a list of names and values.
    fn write(&self, writer: &mut Writer) {
        let settings: Vec<(&str, &str)> = self.iter().collect();
        writer.list(&settings, |writer, (name, value)| {
            writer.text(name);
            writer.text(value);
        });
    }

    /// Reads the settings, which must be in the strict order of their names.
    fn read(reader: &mut Reader<'_>) -> Result<Self, CanonicalError> {
        let settings = reader.list(Self::MAX_SETTINGS, "the invariant settings", |reader| {
            let name = reader.text(Self::MAX_NAME_LEN, "the name of a setting")?;
            let value = reader.text(Self::MAX_VALUE_LEN, "the value of a setting")?;
            check_setting(name, value)?;
            Ok((name.to_owned(), value.to_owned()))
        })?;
        if settings.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
            return Err(CanonicalError::new(
                "the invariant settings are not in the strict order of their names",
            ));
        }
        Ok(Self(settings.into_iter().collect()))
    }
}

/// Explains why `name` and `value` are not an invariant setting, if they are not.
fn check_setting(name: &str, value: &str) -> Result<(), CanonicalError> {
    let name_valid = (1..=Invariants::MAX_NAME_LEN).contains(&name.len())
        && name.starts_with(|first: char| first.is_ascii_lowercase())
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        });
    if !name_valid {
        return Err(CanonicalError::new(format!(
            "invariant setting {name:?} must be named by 1 to {} lowercase ASCII letters, digits, \
             '.', '-', or '_', starting with a letter",
            Invariants::MAX_NAME_LEN
        )));
    }
    if value.len() > Invariants::MAX_VALUE_LEN
        || !value.bytes().all(|byte| (0x20..0x7f).contains(&byte))
    {
        return Err(CanonicalError::new(format!(
            "the value of invariant setting {name:?} must hold at most {} printable ASCII \
             characters",
            Invariants::MAX_VALUE_LEN
        )));
    }
    Ok(())
}

/// The CPU profile that a capture requests: OpenVMM's `--cpu-profile`.
fn cpu_profile_problem(profile: &str) -> Option<String> {
    (profile != "auto" && profile != "host" && parse_profile_id(profile).is_none()).then(|| {
        format!(
            "CPU profile {profile:?} must be \"auto\", \"host\", or a profile ID, \
             VENDOR.GENERATION.vREVISION"
        )
    })
}

/// Returns the guest's network on `nic`, its IPv4 address and prefix length, in the canonical form
/// of a [`SandboxNetwork`].
fn guest_network_of(nic: &Nic) -> String {
    format!("{}/{}", nic.ipv4, nic.prefix_length)
}

/// The invariant inputs of a template's capture, known before it: what a template's adapter
/// requests, and the class of the templates that the capture makes.
///
/// A capture whose manifest matches its class makes a template whose [`TemplateKey`] combines
/// them: [`TemplateKey::new`] checks that the machine of the manifest is the class's, and takes
/// the rest of the key from it. Captures of one class make templates of one key, unless something
/// that the class does not fix changed between them, such as the host's CPU, which its effective
/// CPUID and its rates follow.
///
/// Its [`identity`](Self::identity) names the class: the SHA-256 digest of its canonical encoding,
/// which holds every field, and the versions of the contracts that the key holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureClass {
    /// The runtime: the identities of OpenVMM, the guest kernel, and the guest initramfs, and the
    /// versions of the boot and control contracts.
    pub runtime: RuntimeIdentity,
    /// The build of the guest's agent, which the adapter supplies.
    pub guest_build: ArtifactIdentity,
    /// The backend.
    pub hypervisor: Hypervisor,
    /// The CPU profile that the capture requests, as OpenVMM's `--cpu-profile` takes it: `auto`,
    /// for the built-in profile of the host's CPU, `host`, for a profile that the host derives, or
    /// a profile ID, `VENDOR.GENERATION.vREVISION`.
    pub cpu_profile: String,
    /// The processor capacity: 1, 2, 4, or 8.
    pub processors: u32,
    /// The processors online at boot: 1, 2, 4, or 8, at most [`processors`](Self::processors).
    pub boot_processors: u32,
    /// The RAM at boot, in bytes.
    pub memory: u64,
    /// The RAM capacity, in bytes, to which a restore may expand the RAM: at least
    /// [`memory`](Self::memory).
    pub memory_capacity: u64,
    /// The placeholder `distro` that the capture attaches: its length is the capacity of every
    /// clone's image.
    pub distro: BlockGeometry,
    /// The dormant scratch device.
    pub scratch: BlockGeometry,
    /// The network: `None` or a network that OpenVMM attaches no device for, as
    /// [`SandboxNetwork::openvmm_arguments`] lowers it, for a template without a NIC, and
    /// otherwise a network at the address of the template's fixed NIC. Of the network, the class
    /// takes only what the template captures: its NIC, at the guest's address, under the
    /// complete deny-all policy, as [`SandboxNetwork::template_arguments`] lowers it, whatever
    /// the network's own policy, which each clone's restore replaces with its own. So the classes
    /// of two networks at one address, whose policies both attach a NIC, are one.
    pub network: Option<SandboxNetwork>,
    /// The invariant settings beyond the machine.
    pub invariants: Invariants,
}

impl CaptureClass {
    /// Version of the canonical encoding of a class, its first two bytes.
    pub const VERSION: u16 = 1;

    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) unless every field
    /// has the form that its documentation gives, the runtime identity passes
    /// [`RuntimeIdentity::check`], and the network lowers to OpenVMM's arguments.
    pub fn check(&self) -> Result<()> {
        self.runtime.check()?;
        if let Some(problem) = cpu_profile_problem(&self.cpu_profile) {
            return Err(Error::unsupported(problem));
        }
        if !PROCESSOR_COUNTS.contains(&self.processors)
            || !PROCESSOR_COUNTS.contains(&self.boot_processors)
            || self.boot_processors > self.processors
        {
            return Err(Error::unsupported(format!(
                "a capture of {} processors with {} online at boot needs 1, 2, 4, or 8 of each, \
                 and no more online than in all",
                self.processors, self.boot_processors
            )));
        }
        if self.memory == 0 || self.memory_capacity < self.memory {
            return Err(Error::unsupported(format!(
                "a capture of {} bytes of RAM needs a nonzero RAM and a capacity of at least it, \
                 not {}",
                self.memory, self.memory_capacity
            )));
        }
        for (role, geometry) in [("distro", &self.distro), ("scratch", &self.scratch)] {
            if let Some(problem) = geometry.problem() {
                return Err(Error::unsupported(format!(
                    "the {role} device of a capture is invalid: its {problem}"
                )));
            }
        }
        self.network_arguments()?;
        Ok(())
    }

    /// Returns OpenVMM's arguments of the template's NIC, as
    /// [`SandboxNetwork::template_arguments`] lowers the network, none without a NIC.
    fn network_arguments(&self) -> Result<Vec<String>> {
        match &self.network {
            None => Ok(Vec::new()),
            Some(network) => network.template_arguments(),
        }
    }

    /// Returns the canonical encoding of the class, which its [`identity`](Self::identity)
    /// digests.
    ///
    /// Version 1 holds, in order: [`VERSION`](Self::VERSION) as a `u16`; the runtime identity's
    /// OpenVMM, kernel, and initramfs identities as texts, its boot contract version as a `u64`,
    /// its control contract revision as a text, and its control session version as a `u64`; the
    /// guest build as a text; the snapshot format, device-state schema, time ABI, restore packet,
    /// and activation contract versions as `u64`s; the backend, as OpenVMM spells it, and the
    /// CPU profile, as texts; the processors and those online at boot as `u32`s; the RAM and its
    /// capacity as `u64`s; the `distro` and scratch geometries, each a `u64` length and two
    /// `u32` block sizes; the network, as the list of the texts of OpenVMM's arguments of the
    /// template's NIC, which [`SandboxNetwork::template_arguments`] lowers it to, empty without a
    /// NIC; and the invariant settings, as a list of names and values, each a text, in the order
    /// of their names. The [module](super) describes the encoding of each kind of value.
    ///
    /// Fails as [`check`](Self::check) does.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.encode(&Contracts::CURRENT)
    }

    /// Returns the canonical encoding of the class, with the contract versions `contracts`.
    fn encode(&self, contracts: &Contracts) -> Result<Vec<u8>> {
        self.check()?;
        let mut writer = Writer::default();
        writer.u16(Self::VERSION);
        write_runtime(&mut writer, &self.runtime);
        writer.text(self.guest_build.as_str());
        contracts.write(&mut writer);
        writer.text(self.hypervisor.as_str());
        writer.text(&self.cpu_profile);
        writer.u32(self.processors);
        writer.u32(self.boot_processors);
        writer.u64(self.memory);
        writer.u64(self.memory_capacity);
        write_geometry(&mut writer, &self.distro);
        write_geometry(&mut writer, &self.scratch);
        writer.list(&self.network_arguments()?, |writer, argument| {
            writer.text(argument)
        });
        self.invariants.write(&mut writer);
        Ok(writer.finish())
    }

    /// Returns the identity of the class.
    ///
    /// Fails as [`check`](Self::check) does.
    pub fn identity(&self) -> Result<ClassIdentity> {
        Ok(ClassIdentity(digest(CLASS_DOMAIN, &self.to_bytes()?)))
    }
}

/// The compatibility key of a template: a canonical, versioned projection of the small metadata
/// that decides which clones a template can serve, and on which hosts.
///
/// A key holds, and nothing else:
///
/// - **Bundle and contracts:** the identities of OpenVMM, the guest kernel, and the guest
///   initramfs, and the build of the guest's agent, which the adapter supplies; and the versions
///   of the key's encoding and of the boot, snapshot, device-state, time, restore-packet,
///   control, and activation contracts.
/// - **CPU and time:** the backend; the CPU profile's vendor, generation, revision, and the digest
///   of its canonical document, and the effective CPUID; the processor topology; and the TSC and
///   LAPIC timer rates that the time contract declares, and its rate policy, the tolerance of a
///   restoring host's TSC rate. The time ABI version is the time contract's.
/// - **Resources:** the processor capacity and the processors online at boot; the RAM at boot,
///   its capacity, the size of the blocks in which a restore expands it, and the ranges of both.
/// - **Boot:** the exact effective kernel command line, and its digest.
/// - **Disks:** the length and block sizes of the `distro`, which is the capacity of every clone's
///   image, and of the scratch.
/// - **Devices and isolation:** each device's ID, kind, state unit, transport, address ranges,
///   interrupt, features, and queues, and the interrupt delivery of the virtio devices; the
///   filesystem slot's version and tag; whether there is a NIC, and the fixed NIC's profile, MAC
///   addresses, addresses, prefixes, and gateways, with the identity of the network policy that
///   OpenVMM enforced there at the capture, the complete deny-all policy, which every restore of
///   the template replaces with its clone's own; and the template's [`Invariants`], its other
///   invariant isolation and resource settings.
///
/// A capture's own metadata is not part of it, so the generations of one key differ in it: when
/// the capture was taken, its time contract's anchor, host, boot, and lineage counter, and the
/// lengths of its artifacts. [`TemplateManifest`] keeps them. Nothing of a clone is part of it
/// either: its image, upper, export, network policy and settings, identity, and activation
/// targets. So a template with a NIC serves the clones of every network policy at its guest's
/// address.
///
/// The key comes from metadata alone: the artifacts' identities that their install metadata
/// records, the class's inputs, and the capture's manifest. It never reads or digests a binary,
/// an image, the RAM, or the saved state. A bundle that changes any of its artifacts therefore
/// makes templates of a new key: the templates of the old one are not recertified.
///
/// Its [`identity`](Self::identity) is the SHA-256 digest of its
/// [canonical encoding](Self::to_bytes), after a domain that no other digest of this crate shares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateKey {
    /// The runtime: OpenVMM, the guest kernel and initramfs, and the boot and control contracts.
    runtime: RuntimeIdentity,
    /// The build of the guest's agent.
    guest_build: ArtifactIdentity,
    /// The versions of the other contracts.
    contracts: Contracts,
    /// The machine, as the manifest describes it.
    machine: Machine,
    /// The tag of the filesystem slot.
    filesystem_tag: String,
    /// The invariant settings beyond the machine.
    invariants: Invariants,
}

impl TemplateKey {
    /// Version of the canonical encoding of a key, its first two bytes.
    ///
    /// Version 1 holds the template's NIC, if it has one, with the identity of the complete
    /// deny-all policy under which the capture attached it, but no network policy of its clones.
    /// It has not been released, so a template store that an earlier build created is not
    /// supported: delete it and create a new one. There is no migration.
    pub const VERSION: u16 = 1;

    /// Most bytes of a key's canonical encoding: what the largest key that a manifest of at most
    /// [`MAX_MANIFEST_LEN`](super::MAX_MANIFEST_LEN) bytes can describe takes, with room to spare.
    pub const MAX_LEN: usize = 256 * 1024;

    /// Returns the key of the template that a capture of `class` made, whose manifest is
    /// `manifest`.
    ///
    /// The manifest's machine must be the class's: the backend, the processors and those online
    /// at boot, the RAM and its capacity, the `distro` and scratch geometries, the presence of a
    /// NIC, and the NIC's address and prefix, and a CPU profile that the class's request selects.
    /// A manifest whose machine has no processors online at boot activates all of them at boot,
    /// which a class whose processors are all online at boot requests. The key takes the class's
    /// runtime, guest build, and invariant settings, and this crate's versions of the other
    /// contracts.
    ///
    /// The key also takes the identity of the network policy that OpenVMM enforced on the NIC,
    /// which this check leaves to OpenVMM: OpenVMM derives it from the arguments of the
    /// template's NIC, under the complete deny-all policy, as
    /// [`SandboxNetwork::template_arguments`] lowers the class's network, by an encoding of its
    /// own, which this crate does not reproduce. A capture that enforced another policy makes a
    /// key of its own, which no restore serves, since OpenVMM lets a restore replace the complete
    /// deny-all policy alone.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) as
    /// [`CaptureClass::check`] does, and with [`ErrorKind::Io`](crate::ErrorKind::Io) for a
    /// manifest whose machine is not the class's, as a capture that did not do what its class
    /// requested.
    pub fn new(class: &CaptureClass, manifest: &TemplateManifest) -> Result<Self> {
        class.check()?;
        let machine = &manifest.machine;
        let mismatch = |found: String, requested: String| {
            Err(Error::io(format!(
                "the capture does not match its class: {found}, but the class requests \
                 {requested}"
            )))
        };
        if machine.hypervisor != class.hypervisor {
            return mismatch(
                format!("its backend is {}", machine.hypervisor),
                class.hypervisor.to_string(),
            );
        }
        let profile = machine.cpu.id();
        // `auto` selects the built-in profile of the host's CPU, or falls back to a profile that
        // the host derives, whose generation is `host`, when no built-in profile serves it.
        let selected = match class.cpu_profile.as_str() {
            "auto" => true,
            "host" => machine.cpu.generation == HOST_GENERATION,
            requested => profile == requested,
        };
        if !selected {
            return mismatch(
                format!("its CPU profile is {profile}"),
                class.cpu_profile.clone(),
            );
        }
        let boot_processors = match machine.boot_processors {
            0 => machine.processors,
            count => count,
        };
        if (machine.processors, boot_processors) != (class.processors, class.boot_processors) {
            let describe =
                |processors, online| format!("{processors} processors, {online} online at boot");
            return mismatch(
                format!("it has {}", describe(machine.processors, boot_processors)),
                describe(class.processors, class.boot_processors),
            );
        }
        if (machine.memory.boot, machine.memory.capacity) != (class.memory, class.memory_capacity) {
            let describe =
                |memory, capacity| format!("{memory} bytes of RAM, of a capacity of {capacity}");
            return mismatch(
                format!(
                    "it has {}",
                    describe(machine.memory.boot, machine.memory.capacity)
                ),
                describe(class.memory, class.memory_capacity),
            );
        }
        for (role, found, requested) in [
            ("distro", &machine.distro, &class.distro),
            ("scratch", &machine.scratch, &class.scratch),
        ] {
            if found != requested {
                return mismatch(
                    format!("its {role} geometry is {found:?}"),
                    format!("{requested:?}"),
                );
            }
        }
        let attached = !class.network_arguments()?.is_empty();
        let requested_network = class
            .network
            .as_ref()
            .filter(|_| attached)
            .map(|network| network.guest_network().to_owned());
        // A network that `SandboxNetwork` accepts has the canonical form that this writes.
        let found_network = machine.nic.as_ref().map(guest_network_of);
        if found_network != requested_network {
            let describe = |network: Option<String>| network.unwrap_or_else(|| "none".to_owned());
            return mismatch(
                format!("its NIC network is {}", describe(found_network)),
                describe(requested_network),
            );
        }
        Ok(Self {
            runtime: class.runtime.clone(),
            guest_build: class.guest_build.clone(),
            contracts: Contracts::CURRENT,
            machine: machine.clone(),
            filesystem_tag: MOUNT_TAG.to_owned(),
            invariants: class.invariants.clone(),
        })
    }

    /// Returns the identity of the key.
    pub fn identity(&self) -> KeyIdentity {
        KeyIdentity(digest(KEY_DOMAIN, &self.to_bytes()))
    }

    /// Returns the runtime: the identities of OpenVMM, the guest kernel, and the guest initramfs,
    /// and the versions of the boot and control contracts.
    pub fn runtime(&self) -> &RuntimeIdentity {
        &self.runtime
    }

    /// Returns the build of the guest's agent.
    pub fn guest_build(&self) -> &ArtifactIdentity {
        &self.guest_build
    }

    /// Returns whether the versions of the snapshot, device-state, time, restore-packet, and
    /// activation contracts in the key are those that this crate speaks.
    pub fn speaks_current_contracts(&self) -> bool {
        self.contracts == Contracts::CURRENT
    }

    /// Returns the backend.
    pub fn hypervisor(&self) -> Hypervisor {
        self.machine.hypervisor
    }

    /// Returns the ID of the CPU profile, `VENDOR.GENERATION.vREVISION`.
    pub fn cpu_profile(&self) -> String {
        self.machine.cpu.id()
    }

    /// Returns the processor capacity.
    pub fn processors(&self) -> u32 {
        self.machine.processors
    }

    /// Returns the processors online at boot, or 0 when a restore activates none, as when they
    /// are all online at boot.
    pub fn boot_processors(&self) -> u32 {
        self.machine.boot_processors
    }

    /// Returns the RAM at boot, in bytes.
    pub fn memory(&self) -> u64 {
        self.machine.memory.boot
    }

    /// Returns the RAM capacity, in bytes.
    pub fn memory_capacity(&self) -> u64 {
        self.machine.memory.capacity
    }

    /// Returns the size of the blocks in which a restore expands the RAM, in bytes.
    pub fn memory_block_size(&self) -> u64 {
        self.machine.memory.block_size
    }

    /// Returns the exact effective kernel command line.
    pub fn command_line(&self) -> &str {
        &self.machine.command_line
    }

    /// Returns the geometry of the `distro`, whose length is the capacity of every clone's image.
    pub fn distro(&self) -> BlockGeometry {
        self.machine.distro
    }

    /// Returns the geometry of the scratch.
    pub fn scratch(&self) -> BlockGeometry {
        self.machine.scratch
    }

    /// Returns whether the template has a NIC.
    pub fn has_network(&self) -> bool {
        self.machine.nic.is_some()
    }

    /// Returns the guest's network on the template's NIC, its IPv4 address and prefix length, in
    /// the canonical form of a [`SandboxNetwork`], such as `10.0.0.2/24`, which OpenVMM restores
    /// from the snapshot with the NIC, or `None` for a template without a NIC. The template serves
    /// the clones of every network policy at that address.
    pub fn guest_network(&self) -> Option<String> {
        self.machine.nic.as_ref().map(guest_network_of)
    }

    /// Returns the gateway of the IPv6 identity of the template's NIC, or `None` for a template
    /// without a NIC or whose NIC has no IPv6 identity.
    pub(crate) fn ipv6_gateway(&self) -> Option<Ipv6Addr> {
        self.machine
            .nic
            .as_ref()?
            .ipv6
            .as_ref()
            .map(|ipv6| ipv6.gateway)
    }

    /// Returns the tag of the filesystem slot, on which a clone's export is bound.
    pub fn filesystem_tag(&self) -> &str {
        &self.filesystem_tag
    }

    /// Returns the invariant settings beyond the machine.
    pub fn invariants(&self) -> &Invariants {
        &self.invariants
    }

    /// Returns the canonical encoding of the key, which its [`identity`](Self::identity) digests,
    /// and [`from_bytes`](Self::from_bytes) reads back.
    ///
    /// Version 1 holds, in order, each value encoded as the [module](super) describes:
    ///
    /// 1. [`VERSION`](Self::VERSION), a `u16`.
    /// 2. The bundle and contracts: the OpenVMM, kernel, and initramfs identities, texts; the boot
    ///    contract version, a `u64`; the control contract revision, a text; the control session
    ///    version, a `u64`; the guest build, a text; and the snapshot format, device-state schema,
    ///    time ABI, restore packet, and activation contract versions, `u64`s.
    /// 3. CPU and time: the backend, a text, as OpenVMM spells it; the CPU profile's vendor and
    ///    generation, texts, its revision, a `u32`, and its digest, 32 bytes; the effective
    ///    CPUID, a byte string, as OpenVMM encodes it; the sockets, the dies of each socket, the
    ///    cores of each die, and the threads of each core, `u32`s, and the APIC IDs, a list of
    ///    `u32`s; the TSC rate and the LAPIC timer rate in Hz, `u64`s, and the rate tolerance in
    ///    parts per million, a `u32`.
    /// 4. Resources: the processor capacity and the processors online at boot, `u32`s; the RAM at
    ///    boot, its capacity, and its block size, `u64`s; and the ranges of the RAM at boot, then
    ///    those that a restore may add, each a list of ranges of a `u64` start and a `u64` length.
    /// 5. Boot: the effective command line, a text, and its SHA-256 digest, 32 bytes.
    /// 6. Disks: the `distro`'s length, a `u64`, and its logical and physical block sizes, `u32`s;
    ///    then the scratch's.
    /// 7. Devices and isolation: the interrupt delivery mode, a text, and the start and length of
    ///    the shared interrupt-status page, `u64`s; the devices, a list, each with its stable ID,
    ///    kind, state unit, and transport, texts, its address ranges, a list of an address space,
    ///    `mmio` or `pmio`, a text, a start, and a length, `u64`s, its optional interrupt, a
    ///    `u32`, its feature banks, a list of `u32`s, its queue count, a `u32`, and the largest
    ///    size of each queue, a list of `u32`s; the filesystem slot's version, a `u32`, and tag,
    ///    a text; the optional NIC, with its profile, a text, the guest's and the gateway's MAC
    ///    addresses, 6 bytes each, the guest's IPv4 address, 4 bytes, its prefix length, a `u8`,
    ///    the gateway's IPv4 address, 4 bytes, the optional IPv6 identity, with the guest's
    ///    address, 16 bytes, its prefix length, a `u8`, and the gateway's address, 16 bytes, and
    ///    the network policy's mode, a text, digest, 32 bytes, whether it is required, a boolean,
    ///    and encoding version, a `u32`; and the invariant settings, a list of names and values,
    ///    each a text, in the order of their names.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut writer = Writer::default();
        writer.u16(Self::VERSION);
        write_runtime(&mut writer, &self.runtime);
        writer.text(self.guest_build.as_str());
        self.contracts.write(&mut writer);
        write_machine(&mut writer, &self.machine, &self.filesystem_tag);
        self.invariants.write(&mut writer);
        writer.finish()
    }

    /// Reads a key from its canonical encoding, as [`to_bytes`](Self::to_bytes) writes it.
    ///
    /// The bytes must be the key's one encoding, of version [`VERSION`](Self::VERSION), with no
    /// value of another form than a key derived from a manifest has, and no byte left over.
    ///
    /// Fails with [`ErrorCode::FormatUnsupported`](crate::ErrorCode::FormatUnsupported) for a key
    /// of another version, and with [`ErrorKind::Io`](crate::ErrorKind::Io) for bytes that are
    /// not such a key.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > Self::MAX_LEN {
            return Err(Error::io(format!(
                "a template key of {} bytes is longer than the {} bytes that a key may hold",
                bytes.len(),
                Self::MAX_LEN
            )));
        }
        let mut reader = Reader::new(bytes);
        let malformed =
            |error: CanonicalError| Error::io(format!("the template key is malformed: {error}"));
        let version = reader.u16("the version").map_err(malformed)?;
        if version != Self::VERSION {
            return Err(Error::new(
                crate::ErrorCode::FormatUnsupported,
                format!(
                    "the template key has encoding version {version}; this build reads version {} \
                     only",
                    Self::VERSION
                ),
            ));
        }
        let read = |reader: &mut Reader<'_>| -> Result<Self, CanonicalError> {
            let runtime = read_runtime(reader)?;
            let guest_build = read_artifact(reader, "the guest build")?;
            let contracts = Contracts::read(reader)?;
            let (machine, filesystem_tag) = read_machine(reader)?;
            let invariants = Invariants::read(reader)?;
            Ok(Self {
                runtime,
                guest_build,
                contracts,
                machine,
                filesystem_tag,
                invariants,
            })
        };
        let key = read(&mut reader).map_err(malformed)?;
        reader.finish().map_err(malformed)?;
        Ok(key)
    }
}

/// Writes the machine `machine`, whose filesystem slot has the tag `tag`, as
/// [`TemplateKey::to_bytes`] describes it.
fn write_machine(writer: &mut Writer, machine: &Machine, tag: &str) {
    writer.text(machine.hypervisor.as_str());
    let cpu = &machine.cpu;
    writer.text(&cpu.vendor);
    writer.text(&cpu.generation);
    writer.u32(cpu.revision);
    writer.fixed(&cpu.digest);
    writer.bytes(&cpu.effective_cpuid);
    let topology = &machine.topology;
    for count in [
        topology.sockets,
        topology.dies_per_socket,
        topology.cores_per_die,
        topology.threads_per_core,
    ] {
        writer.u32(count);
    }
    writer.list(&topology.apic_ids, |writer, id| writer.u32(*id));
    writer.u64(machine.rates.tsc_hz);
    writer.u64(machine.rates.apic_hz);
    writer.u32(machine.rates.tolerance_ppm);
    writer.u32(machine.processors);
    writer.u32(machine.boot_processors);
    let memory = &machine.memory;
    writer.u64(memory.boot);
    writer.u64(memory.capacity);
    writer.u64(memory.block_size);
    for ranges in [&memory.ranges, &memory.expansion] {
        writer.list(ranges, |writer, range| {
            writer.u64(range.start);
            writer.u64(range.length);
        });
    }
    writer.text(&machine.command_line);
    writer.fixed(&machine.command_line_digest);
    write_geometry(writer, &machine.distro);
    write_geometry(writer, &machine.scratch);
    writer.text(&machine.interrupts.mode);
    writer.u64(machine.interrupts.status_page_start);
    writer.u64(machine.interrupts.status_page_length);
    writer.list(&machine.devices, |writer, device| {
        for text in [
            &device.stable_id,
            &device.kind,
            &device.state_unit,
            &device.transport,
        ] {
            writer.text(text);
        }
        writer.list(&device.ranges, |writer, range| {
            writer.text(range.space.as_str());
            writer.u64(range.start);
            writer.u64(range.length);
        });
        writer.option(device.irq.as_ref(), |writer, irq| writer.u32(*irq));
        writer.list(&device.feature_banks, |writer, bank| writer.u32(*bank));
        writer.u32(device.queue_count);
        writer.list(&device.queue_max_sizes, |writer, size| writer.u32(*size));
    });
    writer.u32(machine.filesystem_slot);
    writer.text(tag);
    writer.option(machine.nic.as_ref(), |writer, nic| {
        writer.text(&nic.profile);
        writer.fixed(&nic.guest_mac);
        writer.fixed(&nic.gateway_mac);
        writer.fixed(&nic.ipv4.octets());
        writer.u8(nic.prefix_length);
        writer.fixed(&nic.gateway.octets());
        writer.option(nic.ipv6.as_ref(), |writer, ipv6| {
            writer.fixed(&ipv6.address.octets());
            writer.u8(ipv6.prefix_length);
            writer.fixed(&ipv6.gateway.octets());
        });
        writer.text(&nic.policy.mode);
        writer.fixed(&nic.policy.digest);
        writer.bool(nic.policy.required);
        writer.u32(nic.policy.encoding_version);
    });
}

/// Reads a machine and the tag of its filesystem slot, as [`write_machine`] writes them, and
/// checks the machine as a manifest's is checked.
fn read_machine(reader: &mut Reader<'_>) -> Result<(Machine, String), CanonicalError> {
    let short = |reader: &mut Reader<'_>, what: &str| -> Result<String, CanonicalError> {
        Ok(reader.text(MAX_TEXT_LEN, what)?.to_owned())
    };
    let backend = short(reader, "the backend")?;
    let hypervisor = backend
        .parse::<Hypervisor>()
        .map_err(|_| CanonicalError::new(format!("the backend {backend:?} is unknown")))?;
    let cpu = CpuProfile {
        vendor: short(reader, "the CPU vendor")?,
        generation: short(reader, "the CPU generation")?,
        revision: reader.u32("the CPU profile revision")?,
        digest: reader.fixed("the CPU profile digest")?,
        effective_cpuid: reader
            .bytes(MAX_CPUID_ENTRIES * CPUID_ENTRY_LEN, "the effective CPUID")?
            .to_vec(),
    };
    let topology = Topology {
        sockets: reader.u32("the sockets")?,
        dies_per_socket: reader.u32("the dies of each socket")?,
        cores_per_die: reader.u32("the cores of each die")?,
        threads_per_core: reader.u32("the threads of each core")?,
        apic_ids: reader.list(MAX_PROCESSORS, "the APIC IDs", |reader| {
            reader.u32("an APIC ID")
        })?,
    };
    let rates = Rates {
        tsc_hz: reader.u64("the TSC rate")?,
        apic_hz: reader.u64("the LAPIC timer rate")?,
        tolerance_ppm: reader.u32("the rate tolerance")?,
    };
    let processors = reader.u32("the processor capacity")?;
    let boot_processors = reader.u32("the processors online at boot")?;
    let boot = reader.u64("the RAM")?;
    let capacity = reader.u64("the RAM capacity")?;
    let block_size = reader.u64("the RAM block size")?;
    let mut range_lists = Vec::with_capacity(2);
    for what in ["the RAM ranges", "the RAM expansion ranges"] {
        range_lists.push(reader.list(MAX_MEMORY_RANGES, what, |reader| {
            Ok(MemoryRange {
                start: reader.u64(what)?,
                length: reader.u64(what)?,
            })
        })?);
    }
    let expansion = range_lists.pop().expect("two lists were read");
    let ranges = range_lists.pop().expect("two lists were read");
    let command_line = reader
        .text(MAX_COMMAND_LINE_LEN, "the command line")?
        .to_owned();
    let command_line_digest = reader.fixed("the command line's digest")?;
    let distro = read_geometry(reader, "the distro geometry")?;
    let scratch = read_geometry(reader, "the scratch geometry")?;
    let interrupts = Interrupts {
        mode: short(reader, "the interrupt delivery mode")?,
        status_page_start: reader.u64("the interrupt-status page")?,
        status_page_length: reader.u64("the interrupt-status page")?,
    };
    let devices = reader.list(MAX_DEVICES, "the devices", |reader| {
        let stable_id = short(reader, "a device ID")?;
        let kind = short(reader, "a device kind")?;
        let state_unit = short(reader, "a device's state unit")?;
        let transport = short(reader, "a device's transport")?;
        let ranges = reader.list(MAX_DEVICE_RANGES, "a device's ranges", |reader| {
            let space = reader.text(MAX_TEXT_LEN, "an address space")?;
            let space = AddressSpace::parse(space).ok_or_else(|| {
                CanonicalError::new(format!("the address space {space:?} is unknown"))
            })?;
            Ok(DeviceRange {
                space,
                start: reader.u64("a device range")?,
                length: reader.u64("a device range")?,
            })
        })?;
        Ok(Device {
            stable_id,
            kind,
            state_unit,
            transport,
            ranges,
            irq: reader.option("a device's interrupt", |reader| {
                reader.u32("a device's interrupt")
            })?,
            feature_banks: reader.list(MAX_DEVICE_WORDS, "a device's features", |reader| {
                reader.u32("a feature bank")
            })?,
            queue_count: reader.u32("a device's queue count")?,
            queue_max_sizes: reader.list(MAX_DEVICE_WORDS, "a device's queue sizes", |reader| {
                reader.u32("a queue size")
            })?,
        })
    })?;
    let filesystem_slot = reader.u32("the filesystem slot version")?;
    let tag = short(reader, "the filesystem tag")?;
    let nic = reader.option("the NIC", |reader| {
        let profile = short(reader, "the network profile")?;
        let guest_mac = reader.fixed("the guest's MAC address")?;
        let gateway_mac = reader.fixed("the gateway's MAC address")?;
        let ipv4 = Ipv4Addr::from(reader.fixed::<4>("the guest's IPv4 address")?);
        let prefix_length = reader.u8("the IPv4 prefix length")?;
        let gateway = Ipv4Addr::from(reader.fixed::<4>("the IPv4 gateway")?);
        let ipv6 = reader.option("the IPv6 identity", |reader| {
            Ok(Ipv6Identity {
                address: Ipv6Addr::from(reader.fixed::<16>("the guest's IPv6 address")?),
                prefix_length: reader.u8("the IPv6 prefix length")?,
                gateway: Ipv6Addr::from(reader.fixed::<16>("the IPv6 gateway")?),
            })
        })?;
        let policy = PolicyIdentity {
            mode: short(reader, "the network policy's mode")?,
            digest: reader.fixed("the network policy's digest")?,
            required: reader.bool("whether the network policy is required")?,
            encoding_version: reader.u32("the network policy's encoding version")?,
        };
        Ok(Nic {
            profile,
            guest_mac,
            gateway_mac,
            ipv4,
            prefix_length,
            gateway,
            ipv6,
            policy,
        })
    })?;
    let machine = Machine {
        hypervisor,
        cpu,
        topology,
        rates,
        processors,
        boot_processors,
        memory: Memory {
            boot,
            capacity,
            block_size,
            ranges,
            expansion,
        },
        command_line,
        command_line_digest,
        distro,
        scratch,
        interrupts,
        devices,
        filesystem_slot,
        nic,
    };
    if let Some(problem) = machine.problem() {
        return Err(CanonicalError::new(format!("the machine {problem}")));
    }
    if machine.filesystem_slot != FILESYSTEM_SLOT_VERSION || tag.is_empty() {
        return Err(CanonicalError::new(
            "the filesystem slot is not the dormant slot of version 1, with a tag",
        ));
    }
    Ok((machine, tag))
}

#[cfg(test)]
mod tests;
