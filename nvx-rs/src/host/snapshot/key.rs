//! The compatibility key of a snapshot, [`SnapshotKey`].

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

use sha2::{Digest, Sha256};

use super::CaptureContext;
use super::canonical::{CanonicalError, Reader, Writer};
use super::manifest::{
    AddressSpace, BlockGeometry, CPUID_ENTRY_LEN, CpuProfile, Device, DeviceRange,
    FILESYSTEM_SLOT_VERSION, HostClock, Interrupts, Ipv6Identity, MAX_COMMAND_LINE_LEN,
    MAX_CPUID_ENTRIES, MAX_DEVICE_RANGES, MAX_DEVICE_WORDS, MAX_DEVICES, MAX_MEMORY_RANGES,
    MAX_PROCESSORS, MAX_TEXT_LEN, Machine, Memory, MemoryRange, Nic, PolicyIdentity, Rates,
    SnapshotManifest, TimeContract, Topology, parse_profile_id,
};
use crate::contract;
use crate::error::{Error, Result};
use crate::guest_restore;
use crate::host::config::{DiskCapture, check_setting};
use crate::host::platform::parse_hex32;
use crate::host::process::BootId;
use crate::host::runtime::{ArtifactIdentity, Hypervisor, MAX_IDENTITY_LEN, RuntimeIdentity};
use crate::host::sandbox::mount::MOUNT_TAG;

pub use crate::host::config::Invariants;

/// The domain of a key's identity: the bytes that precede its canonical encoding in the digest.
const KEY_DOMAIN: &[u8] = b"nvx snapshot key\0";

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
    /// [`guest_restore::VERSION`], as a `u64`, as the other versions are.
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
        activation: guest_restore::VERSION as u64,
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
    /// The compatibility identity of a [`SnapshotKey`]: the SHA-256 digest of its canonical
    /// compatibility prefix, after the domain `nvx snapshot key` and a NUL. The persisted
    /// capture-time anchors do not contribute to it.
    ///
    /// Its text form, which `Display` writes and `FromStr` parses, is 64 lowercase hexadecimal
    /// digits; parsing also accepts uppercase ones.
    KeyIdentity,
    "a template key's identity"
);

/// The canonical encoding of a template's invariant settings, which its key holds.
impl Invariants {
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
            check_setting(name, value).map_err(CanonicalError::new)?;
            Ok((name.to_owned(), value.to_owned()))
        })?;
        if settings.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
            return Err(CanonicalError::new(
                "the invariant settings are not in the strict order of their names",
            ));
        }
        settings
            .iter()
            .try_fold(Self::new(), |settings, (name, value)| {
                settings.with(name, value)
            })
            .map_err(|error| CanonicalError::new(error.message()))
    }
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

/// Returns whether a CPU-profile request names the captured effective profile.
pub(super) fn cpu_profile_matches(profile: &CpuProfile, requested: &str) -> bool {
    match requested {
        "auto" => true,
        "host" => profile.generation == HOST_GENERATION,
        requested => profile.id() == requested,
    }
}

/// Returns the guest's network on `nic`, its IPv4 address and prefix length, in its canonical
/// form.
fn guest_network_of(nic: &Nic) -> String {
    format!("{}/{}", nic.ipv4, nic.prefix_length)
}

/// The compatibility key of a template: a canonical, versioned projection of the small metadata
/// that decides which clones a template can serve, and on which hosts.
///
/// A key holds, and nothing else:
///
/// - **Runtime and contracts:** the identities of OpenVMM, the guest kernel, and the guest
///   initramfs; and the versions
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
/// The complete capture [`TimeContract`] is retained and encoded with the key, including its
/// anchor, host, boot and lineage counter, but those per-capture fields are not part of the
/// compatibility identity. The capture's creation time and artifact lengths remain in
/// [`SnapshotManifest`]. Nothing of a clone is part of the key: its image, upper, export, network
/// policy and settings, identity, or activation targets. So a template with a NIC serves the
/// clones of every network policy at its guest's address.
///
/// The key comes from metadata alone: the artifacts' identities that their install metadata
/// records, the capture's inputs, and its manifest. It never reads or digests a binary,
/// an image, the RAM, or the saved state. A runtime that changes any of its artifacts therefore
/// makes templates of a new key, and restores none of the old key's: they are captured again.
///
/// Its [`identity`](Self::identity) is the SHA-256 digest of its
/// canonical compatibility prefix, after a domain that no other digest of this crate shares.
/// [`to_bytes`](Self::to_bytes) appends the per-capture time fields to that prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotKey {
    /// The runtime: OpenVMM, the guest kernel and initramfs, and the boot and control contracts.
    runtime: RuntimeIdentity,
    /// The versions of the other contracts.
    contracts: Contracts,
    /// The machine, as the manifest describes it.
    machine: Machine,
    /// The tag of the filesystem slot.
    filesystem_tag: String,
    /// The invariant settings beyond the machine.
    invariants: Invariants,
    /// The complete time contract of this particular capture.
    time: TimeContract,
}

/// The checked inputs of a capture, which its context holds.
struct CaptureInputs<'a> {
    /// The runtime's artifact and contract identities.
    runtime: &'a RuntimeIdentity,
    /// The requested backend.
    hypervisor: Hypervisor,
    /// The requested CPU profile.
    cpu_profile: &'a str,
    /// The processor capacity.
    processors: u64,
    /// The processors online at boot.
    boot_processors: u64,
    /// The RAM at boot, in bytes.
    memory: u64,
    /// The RAM capacity, in bytes.
    memory_capacity: u64,
    /// The placeholder image's geometry.
    distro: &'a BlockGeometry,
    /// The dormant scratch's geometry.
    scratch: &'a BlockGeometry,
    /// The resolved NIC address, without a network policy.
    guest_network: Option<String>,
    /// The caller's invariant settings.
    invariants: &'a Invariants,
}

impl SnapshotKey {
    /// Version of the canonical encoding of a key, its first two bytes.
    ///
    /// Version 1 omits the guest build, appends the complete capture-time contract, and holds
    /// the snapshot's NIC, if it has one, with the identity of the complete
    /// deny-all policy under which the capture attached it, but no network policy of its clones.
    /// It has not been released, so no key that an earlier build encoded is supported. There is
    /// no migration.
    pub const VERSION: u16 = 1;

    /// Most bytes of a key's canonical encoding: what the largest key that a manifest of at most
    /// [`MAX_MANIFEST_LEN`](super::MAX_MANIFEST_LEN) bytes can describe takes, with room to spare.
    pub const MAX_LEN: usize = 256 * 1024;

    /// Derives the key from the checked creation context and OpenVMM's validated manifest.
    pub(super) fn from_capture(
        context: &CaptureContext,
        manifest: &SnapshotManifest,
    ) -> Result<Self> {
        context.capture.check()?;
        context.runtime.check()?;
        let cpu_profile = context.cpu_profile.as_deref().unwrap_or("auto");
        if let Some(problem) = cpu_profile_problem(cpu_profile) {
            return Err(Error::unsupported(problem));
        }
        let bytes = |mib: u64| {
            mib.checked_mul(1024 * 1024)
                .ok_or_else(|| Error::unsupported("capture memory in MiB overflows bytes"))
        };
        let DiskCapture::Fresh { image, scratch } = &context.capture.disks;
        Self::from_inputs(
            CaptureInputs {
                runtime: &context.runtime,
                hypervisor: context.hypervisor,
                cpu_profile,
                processors: context.capture.processors.capacity,
                boot_processors: context.capture.processors.boot,
                memory: bytes(context.capture.memory.boot)?,
                memory_capacity: bytes(context.capture.memory.capacity)?,
                distro: image,
                scratch,
                guest_network: context.nic.map(|nic| nic.guest_network.to_string()),
                invariants: &context.capture.invariants,
            },
            manifest,
        )
    }

    /// Checks the manifest's machine against the capture's inputs and takes its effective shape.
    fn from_inputs(class: CaptureInputs<'_>, manifest: &SnapshotManifest) -> Result<Self> {
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
        let selected = cpu_profile_matches(&machine.cpu, class.cpu_profile);
        if !selected {
            return mismatch(
                format!("its CPU profile is {profile}"),
                class.cpu_profile.to_owned(),
            );
        }
        let boot_processors = match machine.boot_processors {
            0 => machine.processors,
            count => count,
        };
        if (u64::from(machine.processors), u64::from(boot_processors))
            != (class.processors, class.boot_processors)
        {
            let describe =
                |processors, online| format!("{processors} processors, {online} online at boot");
            return mismatch(
                format!(
                    "it has {}",
                    describe(u64::from(machine.processors), u64::from(boot_processors))
                ),
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
            ("distro", &machine.distro, class.distro),
            ("scratch", &machine.scratch, class.scratch),
        ] {
            if found != requested {
                return mismatch(
                    format!("its {role} geometry is {found:?}"),
                    format!("{requested:?}"),
                );
            }
        }
        let requested_network = class.guest_network;
        // An `Ipv4Cidr` displays a network in the canonical form that this writes.
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
            contracts: Contracts::CURRENT,
            machine: machine.clone(),
            filesystem_tag: MOUNT_TAG.to_owned(),
            invariants: class.invariants.clone(),
            time: manifest.time().clone(),
        })
    }

    /// Returns the compatibility identity, excluding this capture's time anchors and identities.
    ///
    /// The rates and time ABI remain compatibility inputs. Different capture times may have
    /// different [`to_bytes`](Self::to_bytes) with the same identity.
    pub fn identity(&self) -> KeyIdentity {
        let mut writer = Writer::default();
        self.write_compatibility(&mut writer);
        KeyIdentity(digest(KEY_DOMAIN, &writer.finish()))
    }

    /// Returns the runtime: the identities of OpenVMM, the guest kernel, and the guest initramfs,
    /// and the versions of the boot and control contracts.
    pub fn runtime(&self) -> &RuntimeIdentity {
        &self.runtime
    }

    /// Returns whether the versions of the snapshot, device-state, time, restore-packet, and
    /// activation contracts in the key are those that this crate speaks.
    pub fn speaks_current_contracts(&self) -> bool {
        self.contracts == Contracts::CURRENT
    }

    /// Checks the manifest's complete machine, the current key contracts and the filesystem tag.
    pub(super) fn check_manifest(&self, manifest: &SnapshotManifest) -> Result<()> {
        if self.machine != manifest.machine
            || &self.time != manifest.time()
            || !self.speaks_current_contracts()
            || self.filesystem_tag != MOUNT_TAG
        {
            return Err(Error::io(
                "the snapshot key is not consistent with its manifest and this format's contracts",
            ));
        }
        Ok(())
    }

    /// Returns the backend.
    pub fn hypervisor(&self) -> Hypervisor {
        self.machine.hypervisor
    }

    /// Returns this snapshot's complete capture-time contract, as recorded by its manifest.
    ///
    /// Its capture anchors, host, boot and lineage are persisted, but do not change the
    /// compatibility [`identity`](Self::identity).
    pub fn time(&self) -> &TimeContract {
        &self.time
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
    /// its canonical form, such as `10.0.0.2/24`, which OpenVMM restores from the snapshot with the
    /// NIC, or `None` for a template without a NIC. The template serves
    /// the clones of every network policy at that address.
    pub fn guest_network(&self) -> Option<String> {
        self.machine.nic.as_ref().map(guest_network_of)
    }

    /// Returns the tag of the filesystem slot, on which a clone's export is bound.
    pub fn filesystem_tag(&self) -> &str {
        &self.filesystem_tag
    }

    /// Returns the invariant settings beyond the machine.
    pub fn invariants(&self) -> &Invariants {
        &self.invariants
    }

    /// Returns the canonical persisted encoding, which [`from_bytes`](Self::from_bytes) reads.
    ///
    /// [`identity`](Self::identity) digests the compatibility prefix, items 1 through 8 below,
    /// excluding item 9's per-capture time fields.
    ///
    /// Version 1 holds, in order, each value encoded as the [module](super) describes:
    ///
    /// 1. [`VERSION`](Self::VERSION), a `u16`.
    /// 2. The runtime and contracts: the OpenVMM, kernel, and initramfs identities, texts; the boot
    ///    contract version, a `u64`; the control contract revision, a text; the control session
    ///    version, a `u64`; and the snapshot format, device-state schema,
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
    ///    and encoding version, a `u32`.
    /// 8. The invariant settings, a list of names and values, each a text, in name order.
    /// 9. The capture-specific time contract: the capture TSC, UTC and monotonic anchors,
    ///    `u64`s; the host clock, a text; the host and boot identities, 16 bytes each; the lineage,
    ///    a `u32`; and the optional cut-to-anchor interval, a `u64`. The complete contract's rates
    ///    are already encoded in item 3.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut writer = Writer::default();
        self.write_compatibility(&mut writer);
        write_time(&mut writer, &self.time);
        writer.finish()
    }

    /// Writes exactly the compatibility prefix that the key's identity digests.
    fn write_compatibility(&self, writer: &mut Writer) {
        writer.u16(Self::VERSION);
        write_runtime(writer, &self.runtime);
        self.contracts.write(writer);
        write_machine(writer, &self.machine, &self.filesystem_tag);
        self.invariants.write(writer);
    }

    /// Reads a key from its canonical encoding, as [`to_bytes`](Self::to_bytes) writes it.
    ///
    /// The bytes must be the key's one encoding, of version [`VERSION`](Self::VERSION), with no
    /// value of another form than a key derived from a manifest has, and no byte left over.
    ///
    /// Fails with [`ErrorKind::Io`](crate::ErrorKind::Io) for a key of another version, and for
    /// bytes that are not such a key.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > Self::MAX_LEN {
            return Err(Error::io(format!(
                "a snapshot key of {} bytes is longer than the {} bytes that a key may hold",
                bytes.len(),
                Self::MAX_LEN
            )));
        }
        let mut reader = Reader::new(bytes);
        let malformed =
            |error: CanonicalError| Error::io(format!("the snapshot key is malformed: {error}"));
        let version = reader.u16("the version").map_err(malformed)?;
        if version != Self::VERSION {
            return Err(Error::new(
                crate::ErrorCode::FormatUnsupported,
                format!(
                    "the snapshot key has encoding version {version}; this build reads version {} \
                     only",
                    Self::VERSION
                ),
            ));
        }
        let read = |reader: &mut Reader<'_>| -> Result<Self, CanonicalError> {
            let runtime = read_runtime(reader)?;
            let contracts = Contracts::read(reader)?;
            let (machine, filesystem_tag) = read_machine(reader)?;
            let invariants = Invariants::read(reader)?;
            let time = read_time(reader, machine.rates)?;
            Ok(Self {
                runtime,
                contracts,
                machine,
                filesystem_tag,
                invariants,
                time,
            })
        };
        let key = read(&mut reader).map_err(malformed)?;
        reader.finish().map_err(malformed)?;
        Ok(key)
    }
}

/// Appends the capture-specific fields; the complete contract's rates are in the machine prefix.
fn write_time(writer: &mut Writer, time: &TimeContract) {
    writer.u64(time.capture_tsc);
    writer.u64(time.capture_utc_ns);
    writer.u64(time.capture_monotonic_ns);
    writer.text(time.host_clock.as_str());
    writer.fixed(&time.host_id);
    writer.fixed(time.boot_id.as_bytes());
    writer.u32(time.lineage);
    writer.option(time.cut_to_anchor_ns.as_ref(), |writer, nanos| {
        writer.u64(*nanos)
    });
}

/// Reads the full capture contract using the compatibility prefix's already-checked rates.
fn read_time(reader: &mut Reader<'_>, rates: Rates) -> Result<TimeContract, CanonicalError> {
    let capture_tsc = reader.u64("the capture TSC")?;
    let capture_utc_ns = reader.u64("the capture UTC anchor")?;
    let capture_monotonic_ns = reader.u64("the capture monotonic anchor")?;
    let clock = reader.text(MAX_TEXT_LEN, "the capture host clock")?;
    let host_clock = HostClock::parse(clock).ok_or_else(|| {
        CanonicalError::new(format!("the capture host clock {clock:?} is unknown"))
    })?;
    Ok(TimeContract {
        tsc_frequency_hz: rates.tsc_hz,
        tsc_tolerance_ppm: rates.tolerance_ppm,
        apic_frequency_hz: rates.apic_hz,
        capture_tsc,
        capture_utc_ns,
        capture_monotonic_ns,
        host_clock,
        host_id: reader.fixed("the capturing host identity")?,
        boot_id: BootId::from_bytes(reader.fixed("the capture boot identity")?),
        lineage: reader.u32("the capture lineage")?,
        cut_to_anchor_ns: reader.option("the cut-to-anchor interval", |reader| {
            reader.u64("the cut-to-anchor interval")
        })?,
    })
}

/// Writes the machine `machine`, whose filesystem slot has the tag `tag`, as
/// [`SnapshotKey::to_bytes`] describes it.
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
