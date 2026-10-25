//! The manifest of a template's capture, which [`TemplateManifest::read`] reads.
//!
//! OpenVMM writes `manifest.bin` into the directory of a snapshot that it publishes: one message
//! of the protobuf wire format, as its `mesh` encodes `openvmm.snapshot.SnapshotManifest`, of at
//! most [`MAX_MANIFEST_LEN`] bytes. It describes the machine that the snapshot restores, its time
//! contract and CPU profile, its sandbox blocks, and the lengths of its artifacts. Restore treats
//! it as authoritative, and so does a template's [compatibility key](super::TemplateKey), which
//! projects its machine. [`TemplateManifest`] describes what the reader refuses.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

use super::wire::{Field, Message, WireError};
use crate::contract;
use crate::error::{Error, ErrorCode, Result};
use crate::host::process::BootId;
pub use crate::host::sandbox::geometry::BlockGeometry;
use crate::host::store::regular;
use crate::hypervisor::Hypervisor;

/// Name of the manifest in a snapshot's directory.
pub const MANIFEST_NAME: &str = "manifest.bin";

/// Longest manifest that [`TemplateManifest::read`] reads, in bytes: 1 MiB, the limit of
/// OpenVMM's own reader.
pub const MAX_MANIFEST_LEN: u64 = 1024 * 1024;

/// The magic that identifies a manifest of version 6 of OpenVMM's snapshot format.
const FORMAT_MAGIC: &[u8] = b"OPENVMM_SNAPSHOT_V6\0";

/// The type of the root of a snapshot's saved state.
const SAVED_STATE_ROOT_TYPE: &str = "openvmm.SavedState";

/// The only architecture of a microVM.
const ARCHITECTURE: &str = "x86_64";

/// The page size of a microVM, in bytes.
const PAGE_SIZE: u32 = 4096;

/// The machine profile of a microVM.
const MACHINE_PROFILE: &str = "microvm";

/// Version of the boot layout of a microVM: OpenVMM's Linux-direct boot structures.
const BOOT_LAYOUT_VERSION: u32 = 2;

/// Version of the dormant filesystem slot of a microVM.
pub(super) const FILESYSTEM_SLOT_VERSION: u32 = 1;

/// Version of the restore-time memory expansion of a microVM.
const MEMORY_EXPANSION_VERSION: u32 = 1;

/// The tier of a template's capture.
const TIER_PLATFORM: &str = "platform";

/// The restore policy of a template, which restores independent clones.
const RESTORE_POLICY_CLONE: &str = "clone";

/// The configuration sections that a platform capture consumed: its invariants only.
const CONSUMED_INVARIANTS: u32 = 1;

/// Longest effective kernel command line of a microVM, in bytes, without its NUL.
pub(super) const MAX_COMMAND_LINE_LEN: usize = 2047;

/// Most memory ranges, or memory expansion ranges, of a machine.
pub(super) const MAX_MEMORY_RANGES: usize = 128;

/// Most devices of a machine.
pub(super) const MAX_DEVICES: usize = 64;

/// Most address ranges of one device.
pub(super) const MAX_DEVICE_RANGES: usize = 16;

/// Most feature banks, or queues, of one device.
pub(super) const MAX_DEVICE_WORDS: usize = 64;

/// Longest text of a machine other than its command line, such as a device ID, in bytes.
pub(super) const MAX_TEXT_LEN: usize = 256;

/// Most processors of a machine.
pub(super) const MAX_PROCESSORS: usize = 8;

/// Most sandbox blocks of a machine: one of each role of OpenVMM.
const MAX_BLOCKS: usize = 4;

/// Longest saved state of a snapshot, in bytes: 256 MiB, the limit of OpenVMM's restore.
const MAX_STATE_LEN: u64 = 256 * 1024 * 1024;

/// Most names in a manifest's inventory of state units.
const MAX_STATE_UNITS: usize = 512;

/// Most host attachments in a manifest.
const MAX_ATTACHMENTS: usize = 64;

/// Longest identity of a host attachment, in bytes.
const MAX_ATTACHMENT_IDENTITY_LEN: usize = 4096;

/// Bytes of one entry of a CPU profile's effective CPUID record.
pub(super) const CPUID_ENTRY_LEN: usize = 44;

/// Most entries of a CPU profile's effective CPUID record.
pub(super) const MAX_CPUID_ENTRIES: usize = 1024;

/// The processor counts of a microVM.
pub(super) const PROCESSOR_COUNTS: [u32; 4] = [1, 2, 4, 8];

/// Bytes of a SHA-256 digest.
const SHA256_LEN: usize = 32;

/// Bytes of a host or boot identity.
const HOST_IDENTITY_LEN: usize = 16;

/// Nanoseconds in a second.
const NANOS_PER_SECOND: i32 = 1_000_000_000;

/// The host clock from which a capture took its monotonic anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostClock {
    /// Linux's `CLOCK_BOOTTIME`, `linux-boottime` in a manifest.
    LinuxBoottime,
    /// Windows' interrupt time, in nanoseconds, `windows-interrupt-time` in a manifest.
    WindowsInterruptTime,
}

impl HostClock {
    /// Returns the clock's spelling in a manifest.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LinuxBoottime => "linux-boottime",
            Self::WindowsInterruptTime => "windows-interrupt-time",
        }
    }

    /// Parses the spelling of a clock in a manifest.
    pub(super) fn parse(text: &str) -> Option<Self> {
        match text {
            "linux-boottime" => Some(Self::LinuxBoottime),
            "windows-interrupt-time" => Some(Self::WindowsInterruptTime),
            _ => None,
        }
    }
}

/// The time contract of a capture: the rates that it declares, which its template's key holds,
/// and the anchor at which it was captured, with the identities of the host and the boot that
/// captured it, which the generations of one key differ in.
///
/// A restore uses the anchor to measure how long the capture has been down: on the same host
/// boot, by the host's monotonic clock, and otherwise by UTC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeContract {
    /// The declared TSC rate of the snapshot's lineage, in Hz.
    pub tsc_frequency_hz: u64,
    /// The tolerance of the TSC rate of a restoring host, in parts per million.
    pub tsc_tolerance_ppm: u32,
    /// The rate of the LAPIC timer, in Hz.
    pub apic_frequency_hz: u64,
    /// The TSC of the first virtual processor at the capture anchor.
    pub capture_tsc: u64,
    /// The host's UTC at the capture anchor, in nanoseconds since the Unix epoch.
    pub capture_utc_ns: u64,
    /// The host's monotonic time at the capture anchor, in nanoseconds, on
    /// [`host_clock`](Self::host_clock).
    pub capture_monotonic_ns: u64,
    /// The clock of [`capture_monotonic_ns`](Self::capture_monotonic_ns).
    pub host_clock: HostClock,
    /// The identity of the capturing host: its machine ID on Linux, and its machine GUID on
    /// Windows.
    pub host_id: [u8; 16],
    /// The boot of the capturing host, which a [`BootId`] of the same host compares with.
    pub boot_id: BootId,
    /// The time lineage counter: the generation counter of the captured VM process, which each
    /// restore of the snapshot increments.
    pub lineage: u32,
    /// The host's monotonic time from the cut of the saved VM time to the capture anchor, in
    /// nanoseconds, if the capture recorded it.
    pub cut_to_anchor_ns: Option<u64>,
}

/// The CPU profile of a capture: its ID, `VENDOR.GENERATION.vREVISION`, the digest of its
/// canonical document, and the effective CPUID that the VM saw.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct CpuProfile {
    /// The vendor, such as `amd` or `intel`.
    pub(super) vendor: String,
    /// The generation, such as `genoa`, or `host` for a profile that a host derived.
    pub(super) generation: String,
    /// The revision.
    pub(super) revision: u32,
    /// The SHA-256 digest of the profile's canonical document.
    pub(super) digest: [u8; SHA256_LEN],
    /// The effective CPUID, as OpenVMM encodes it: entries of [`CPUID_ENTRY_LEN`] bytes.
    pub(super) effective_cpuid: Vec<u8>,
}

impl CpuProfile {
    /// Returns the profile's ID: `VENDOR.GENERATION.vREVISION`.
    pub(super) fn id(&self) -> String {
        format!("{}.{}.v{}", self.vendor, self.generation, self.revision)
    }
}

/// Returns whether `text` is one word of a CPU profile ID: 1 to 32 lowercase ASCII letters,
/// digits, or hyphens.
fn is_profile_word(text: &str) -> bool {
    (1..=32).contains(&text.len())
        && text
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// Parses a CPU profile ID, `VENDOR.GENERATION.vREVISION`, whose revision is a positive decimal
/// number without leading zeros, into its vendor, generation, and revision.
pub(super) fn parse_profile_id(id: &str) -> Option<(String, String, u32)> {
    let mut words = id.split('.');
    let (vendor, generation, revision) = (words.next()?, words.next()?, words.next()?);
    let digits = revision.strip_prefix('v')?;
    if words.next().is_some()
        || !is_profile_word(vendor)
        || !is_profile_word(generation)
        || digits.is_empty()
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
        || digits.starts_with('0')
    {
        return None;
    }
    Some((
        vendor.to_owned(),
        generation.to_owned(),
        digits.parse().ok()?,
    ))
}

/// The declared rates of a capture's time contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct Rates {
    /// The declared TSC rate, in Hz.
    pub(super) tsc_hz: u64,
    /// The rate of the LAPIC timer, in Hz.
    pub(super) apic_hz: u64,
    /// The rate policy: the tolerance of a restoring host's TSC rate, in parts per million.
    pub(super) tolerance_ppm: u32,
}

/// The processor topology of a machine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub(super) struct Topology {
    /// Sockets.
    pub(super) sockets: u32,
    /// Dies in each socket.
    pub(super) dies_per_socket: u32,
    /// Cores in each die.
    pub(super) cores_per_die: u32,
    /// Threads in each core.
    pub(super) threads_per_core: u32,
    /// The APIC ID of each processor.
    pub(super) apic_ids: Vec<u32>,
}

/// A range of guest-physical memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct MemoryRange {
    /// Its first guest-physical address.
    pub(super) start: u64,
    /// Its length, in bytes.
    pub(super) length: u64,
}

/// Returns the total length of `ranges`, unless a range is empty or ends beyond 2^64, or their
/// total does.
fn total_length(ranges: &[MemoryRange]) -> Option<u64> {
    ranges.iter().try_fold(0u64, |total, range| {
        if range.length == 0 {
            return None;
        }
        range.start.checked_add(range.length)?;
        total.checked_add(range.length)
    })
}

/// The memory of a machine: the RAM that it boots with, and the capacity to which a restore may
/// expand it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct Memory {
    /// The RAM at boot, in bytes.
    pub(super) boot: u64,
    /// The RAM capacity, in bytes.
    pub(super) capacity: u64,
    /// The size of the blocks in which a restore expands RAM, in bytes.
    pub(super) block_size: u64,
    /// The ranges of the RAM at boot, in the order in which the memory file holds them.
    pub(super) ranges: Vec<MemoryRange>,
    /// The ranges that a restore may add, in their order.
    pub(super) expansion: Vec<MemoryRange>,
}

impl Memory {
    /// Explains why the memory is invalid, if it is.
    fn problem(&self) -> Option<String> {
        if self.ranges.is_empty()
            || self.ranges.len() > MAX_MEMORY_RANGES
            || total_length(&self.ranges) != Some(self.boot)
            || self.boot == 0
        {
            return Some(format!(
                "has {} bytes of RAM in {} ranges that do not hold it",
                self.boot,
                self.ranges.len()
            ));
        }
        if self.block_size == 0
            || self.capacity < self.boot
            || !self.boot.is_multiple_of(self.block_size)
            || !self.capacity.is_multiple_of(self.block_size)
        {
            return Some(format!(
                "has a RAM capacity of {} bytes in blocks of {} bytes for {} bytes of RAM",
                self.capacity, self.block_size, self.boot
            ));
        }
        if self.expansion.len() > MAX_MEMORY_RANGES
            || total_length(&self.expansion) != Some(self.capacity - self.boot)
        {
            return Some(
                "has memory expansion ranges that do not span its RAM capacity".to_owned(),
            );
        }
        None
    }
}

/// The address space of a device's range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum AddressSpace {
    /// Memory-mapped I/O, `mmio`.
    Mmio,
    /// Port I/O, `pmio`.
    Pmio,
}

impl AddressSpace {
    /// Returns the space's spelling in a manifest.
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Mmio => "mmio",
            Self::Pmio => "pmio",
        }
    }

    /// Parses the spelling of a space in a manifest.
    pub(super) fn parse(text: &str) -> Option<Self> {
        match text {
            "mmio" => Some(Self::Mmio),
            "pmio" => Some(Self::Pmio),
            _ => None,
        }
    }
}

/// An address range of a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct DeviceRange {
    /// Its address space.
    pub(super) space: AddressSpace,
    /// Its first address.
    pub(super) start: u64,
    /// Its length.
    pub(super) length: u64,
}

/// A guest-visible device of a machine, in the order of the machine's devices.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct Device {
    /// Its stable ID, unique in the machine.
    pub(super) stable_id: String,
    /// Its kind, such as `virtio-blk`.
    pub(super) kind: String,
    /// The name of the unit of saved state that holds its state.
    pub(super) state_unit: String,
    /// Its transport, such as `virtio-mmio`, or empty for a platform device.
    pub(super) transport: String,
    /// Its address ranges.
    pub(super) ranges: Vec<DeviceRange>,
    /// Its interrupt, if it has one.
    pub(super) irq: Option<u32>,
    /// Its feature banks, 32 bits each.
    pub(super) feature_banks: Vec<u32>,
    /// Its queue count.
    pub(super) queue_count: u32,
    /// The largest size of each of its queues.
    pub(super) queue_max_sizes: Vec<u32>,
}

impl Device {
    /// Explains why the device is invalid, if it is.
    fn problem(&self) -> Option<String> {
        let texts = [&self.stable_id, &self.kind, &self.state_unit];
        if texts
            .into_iter()
            .chain([&self.transport])
            .any(|text| text.len() > MAX_TEXT_LEN)
            || texts.into_iter().any(String::is_empty)
        {
            return Some(format!(
                "has a device without an ID, a kind, or a state unit, or with one of more than \
                 {MAX_TEXT_LEN} bytes: {:?}",
                self.stable_id
            ));
        }
        if self.ranges.len() > MAX_DEVICE_RANGES
            || self.feature_banks.len() > MAX_DEVICE_WORDS
            || self.queue_max_sizes.len() > MAX_DEVICE_WORDS
        {
            return Some(format!(
                "has the device {:?} with more than {MAX_DEVICE_RANGES} ranges, or more than \
                 {MAX_DEVICE_WORDS} feature banks or queues",
                self.stable_id
            ));
        }
        None
    }
}

/// How a machine's virtio devices deliver their interrupts.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct Interrupts {
    /// The delivery mode, such as `edge-shared-status`.
    pub(super) mode: String,
    /// The guest-physical address of the shared interrupt-status page, or 0 without one.
    pub(super) status_page_start: u64,
    /// The length of the shared interrupt-status page, or 0 without one.
    pub(super) status_page_length: u64,
}

impl BlockGeometry {
    /// Explains why the geometry is invalid, if it is, as a phrase whose subject is a size, such
    /// as "length 1000 is not a nonzero multiple of its logical block size".
    pub(super) fn problem(&self) -> Option<String> {
        if self.logical_block_size < 512 || !self.logical_block_size.is_power_of_two() {
            Some(format!(
                "logical block size {} is not a power of two of at least 512",
                self.logical_block_size
            ))
        } else if self.physical_block_size < self.logical_block_size
            || !self.physical_block_size.is_power_of_two()
        {
            Some(format!(
                "physical block size {} is not a power of two of at least its logical block size",
                self.physical_block_size
            ))
        } else if self.length == 0
            || !self
                .length
                .is_multiple_of(u64::from(self.logical_block_size))
        {
            Some(format!(
                "length {} is not a nonzero multiple of its logical block size",
                self.length
            ))
        } else {
            None
        }
    }
}

/// The IPv6 identity of a machine's fixed NIC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct Ipv6Identity {
    /// The guest's address.
    pub(super) address: Ipv6Addr,
    /// The prefix length of the guest's network.
    pub(super) prefix_length: u8,
    /// The gateway's address.
    pub(super) gateway: Ipv6Addr,
}

/// The identity of the network policy that OpenVMM enforces on a machine's fixed NIC.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct PolicyIdentity {
    /// The policy's mode, as OpenVMM names it.
    pub(super) mode: String,
    /// The SHA-256 digest of the policy's canonical encoding, which OpenVMM computed.
    pub(super) digest: [u8; SHA256_LEN],
    /// Whether the policy is enforced.
    pub(super) required: bool,
    /// The version of the policy's canonical encoding, which 0 records for version 1.
    pub(super) encoding_version: u32,
}

/// The fixed NIC of a machine, and the network policy that OpenVMM enforces on it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct Nic {
    /// Its network profile, such as `portable`.
    pub(super) profile: String,
    /// The guest's MAC address.
    pub(super) guest_mac: [u8; 6],
    /// The gateway's MAC address.
    pub(super) gateway_mac: [u8; 6],
    /// The guest's IPv4 address.
    pub(super) ipv4: Ipv4Addr,
    /// The prefix length of the guest's IPv4 network.
    pub(super) prefix_length: u8,
    /// The gateway's IPv4 address.
    pub(super) gateway: Ipv4Addr,
    /// The guest's IPv6 identity, if it has one.
    pub(super) ipv6: Option<Ipv6Identity>,
    /// The network policy.
    pub(super) policy: PolicyIdentity,
}

impl Nic {
    /// Explains why the NIC is invalid, if it is.
    fn problem(&self) -> Option<String> {
        if [&self.profile, &self.policy.mode]
            .into_iter()
            .any(|text| text.is_empty() || text.len() > MAX_TEXT_LEN)
        {
            return Some("has a NIC without a network profile or a policy mode".to_owned());
        }
        if !(1..=30).contains(&self.prefix_length)
            || self
                .ipv6
                .is_some_and(|ipv6| !(1..=128).contains(&ipv6.prefix_length))
        {
            return Some("has a NIC with an invalid prefix length".to_owned());
        }
        None
    }
}

/// The machine of a template's capture, as its manifest describes it: what its key projects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Machine {
    /// The backend that captured it.
    pub(super) hypervisor: Hypervisor,
    /// The CPU profile.
    pub(super) cpu: CpuProfile,
    /// The processor topology.
    pub(super) topology: Topology,
    /// The declared rates of the time contract.
    pub(super) rates: Rates,
    /// The processor capacity.
    pub(super) processors: u32,
    /// The processors online at boot, or 0 when a restore activates none.
    pub(super) boot_processors: u32,
    /// The memory.
    pub(super) memory: Memory,
    /// The effective kernel command line.
    pub(super) command_line: String,
    /// The SHA-256 digest of the effective kernel command line.
    pub(super) command_line_digest: [u8; SHA256_LEN],
    /// The read-only `distro` block device.
    pub(super) distro: BlockGeometry,
    /// The scratch block device.
    pub(super) scratch: BlockGeometry,
    /// The interrupt delivery of the virtio devices.
    pub(super) interrupts: Interrupts,
    /// The devices, in their order.
    pub(super) devices: Vec<Device>,
    /// The version of the dormant filesystem slot.
    pub(super) filesystem_slot: u32,
    /// The fixed NIC, if the machine has one.
    pub(super) nic: Option<Nic>,
}

impl Machine {
    /// Explains why the machine is invalid, if it is, as a phrase whose subject is the machine.
    ///
    /// Both a manifest and a key's canonical encoding must describe a valid machine.
    pub(super) fn problem(&self) -> Option<String> {
        let cpu = &self.cpu;
        if !is_profile_word(&cpu.vendor) || !is_profile_word(&cpu.generation) || cpu.revision == 0 {
            return Some(format!(
                "has the invalid CPU profile vendor {:?}, generation {:?}, and revision {}",
                cpu.vendor, cpu.generation, cpu.revision
            ));
        }
        if cpu.effective_cpuid.is_empty()
            || !cpu.effective_cpuid.len().is_multiple_of(CPUID_ENTRY_LEN)
            || cpu.effective_cpuid.len() > MAX_CPUID_ENTRIES * CPUID_ENTRY_LEN
        {
            return Some(format!(
                "has an effective CPUID of {} bytes, not 1 to {MAX_CPUID_ENTRIES} entries of \
                 {CPUID_ENTRY_LEN} bytes",
                cpu.effective_cpuid.len()
            ));
        }
        if !PROCESSOR_COUNTS.contains(&self.processors)
            || self.topology.apic_ids.len() != self.processors as usize
            || self.boot_processors > self.processors
            || (self.boot_processors != 0 && !PROCESSOR_COUNTS.contains(&self.boot_processors))
        {
            return Some(format!(
                "has {} processors, {} APIC IDs, and {} processors online at boot, which a \
                 microVM cannot have",
                self.processors,
                self.topology.apic_ids.len(),
                self.boot_processors
            ));
        }
        if self.rates.tsc_hz == 0 || self.rates.apic_hz == 0 {
            return Some("declares no TSC or LAPIC timer rate".to_owned());
        }
        if let Some(problem) = self.memory.problem() {
            return Some(problem);
        }
        if self.command_line.len() > MAX_COMMAND_LINE_LEN {
            return Some(format!(
                "has an effective command line of {} bytes, more than the {MAX_COMMAND_LINE_LEN} \
                 that a microVM's command line holds",
                self.command_line.len()
            ));
        }
        if self.command_line_digest != <[u8; SHA256_LEN]>::from(Sha256::digest(&self.command_line))
        {
            return Some("has an effective command line whose digest does not match it".to_owned());
        }
        for (role, geometry) in [("distro", self.distro), ("scratch", self.scratch)] {
            if let Some(problem) = geometry.problem() {
                return Some(format!("has a {role} block whose {problem}"));
            }
        }
        if self.interrupts.mode.is_empty() || self.interrupts.mode.len() > MAX_TEXT_LEN {
            return Some("names no interrupt delivery mode".to_owned());
        }
        if self.devices.is_empty() || self.devices.len() > MAX_DEVICES {
            return Some(format!(
                "has {} devices, not 1 to {MAX_DEVICES}",
                self.devices.len()
            ));
        }
        for (index, device) in self.devices.iter().enumerate() {
            if let Some(problem) = device.problem() {
                return Some(problem);
            }
            if self.devices[..index]
                .iter()
                .any(|other| other.stable_id == device.stable_id)
            {
                return Some(format!(
                    "has the device ID {:?} more than once",
                    device.stable_id
                ));
            }
        }
        self.nic.as_ref().and_then(Nic::problem)
    }
}

/// The manifest of a template's capture, which OpenVMM wrote as `manifest.bin` in the snapshot's
/// directory: the machine that its clones restore, and what the capture recorded of itself.
///
/// [`read`](Self::read) reads it, bounded and strict: it reads at most [`MAX_MANIFEST_LEN`] bytes
/// of a manifest that is a regular file, not a link or other reparse point, and refuses a manifest
/// of any other format version than
/// [`SNAPSHOT_FORMAT_VERSION`](crate::contract::SNAPSHOT_FORMAT_VERSION), a field that this
/// format does not have, a field that is not of its type or that appears twice, truncated bytes,
/// and a manifest that is not a template's: a platform-tier capture of a microVM, for clones, with
/// a read-only `distro` whose identity is unbound, a fresh scratch, and the dormant filesystem
/// slot. It never reads another artifact of the snapshot.
///
/// A template's [`TemplateKey`](super::TemplateKey) projects its machine. The rest is the
/// capture's own: when it was taken, its [`TimeContract`]'s anchor, host, boot, and lineage
/// counter, and the lengths of its artifacts, in which the generations of one key differ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateManifest {
    /// When OpenVMM created the snapshot.
    created_at: SystemTime,
    /// The version of the OpenVMM that captured it.
    openvmm_version: String,
    /// The length of the snapshot's saved state, `state.bin`, in bytes.
    state_len: u64,
    /// The time contract.
    time: TimeContract,
    /// The machine.
    pub(super) machine: Machine,
}

impl TemplateManifest {
    /// Reads the manifest of the snapshot in the directory `snapshot`: its [`MANIFEST_NAME`].
    ///
    /// The file is opened without following a link, or on Windows any other reparse point, and
    /// must be a regular file of at most [`MAX_MANIFEST_LEN`] bytes, which it reads without
    /// reading more. It then checks the manifest as [`parse`](Self::parse) does. The manifest is
    /// named by its path in `snapshot`, so `snapshot` and the directories above it must be ones
    /// that only the user, or an administrator, may change.
    ///
    /// Fails with [`ErrorKind::NotFound`](crate::ErrorKind::NotFound) if the manifest does not
    /// exist, with [`ErrorKind::Io`](crate::ErrorKind::Io) if it cannot be read, or is not a
    /// regular file of at most [`MAX_MANIFEST_LEN`] bytes, and otherwise as
    /// [`parse`](Self::parse) does.
    pub fn read(snapshot: &Path) -> Result<Self> {
        let path = snapshot.join(MANIFEST_NAME);
        let description = format!("the snapshot manifest {}", path.display());
        let bytes = regular::open_regular(&path)
            .and_then(|file| regular::read_at_most(file, MAX_MANIFEST_LEN, "a manifest"))
            .map_err(|error| {
                let code = match error.kind() {
                    io::ErrorKind::NotFound => ErrorCode::NotFound,
                    _ => ErrorCode::Io,
                };
                Error::new(code, format!("cannot read {description}")).with_source(error)
            })?;
        Self::parse_described(&bytes, &description)
    }

    /// Parses and checks the bytes of a template's manifest.
    ///
    /// Fails with [`ErrorCode::FormatUnsupported`] for a
    /// manifest of another version of the snapshot format, saved-state schema, machine ABI, boot
    /// layout, or time ABI than this build reads, and with
    /// [`ErrorKind::Io`](crate::ErrorKind::Io) for more than [`MAX_MANIFEST_LEN`] bytes, bytes
    /// that are not such a manifest, and a manifest that is not a template's, as
    /// [`TemplateManifest`] describes.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        Self::parse_described(bytes, "the snapshot manifest")
    }

    /// Implements [`parse`](Self::parse), with messages that call the manifest `description`.
    fn parse_described(bytes: &[u8], description: &str) -> Result<Self> {
        if bytes.len() as u64 > MAX_MANIFEST_LEN {
            return Err(Error::io(format!(
                "{description} holds {} bytes, more than the {MAX_MANIFEST_LEN} that a manifest \
                 may hold",
                bytes.len()
            )));
        }
        check_format(bytes).map_err(|problem| problem.into_error(description))?;
        let raw = decode_manifest(bytes)
            .map_err(|problem| Error::io(format!("{description} is malformed: {problem}")))?;
        Self::from_raw(raw).map_err(|problem| problem.into_error(description))
    }

    /// Returns when OpenVMM created the snapshot.
    pub fn created_at(&self) -> SystemTime {
        self.created_at
    }

    /// Returns the version of the OpenVMM that captured the snapshot, as it names itself.
    pub fn openvmm_version(&self) -> &str {
        &self.openvmm_version
    }

    /// Returns the backend that captured the snapshot.
    pub fn hypervisor(&self) -> Hypervisor {
        self.machine.hypervisor
    }

    /// Returns the time contract.
    pub fn time(&self) -> &TimeContract {
        &self.time
    }

    /// Returns the length of the snapshot's saved state, `state.bin`, in bytes.
    pub fn state_len(&self) -> u64 {
        self.state_len
    }

    /// Returns the length of the snapshot's RAM, `memory.bin`, in bytes: the RAM at boot.
    pub fn memory_len(&self) -> u64 {
        self.machine.memory.boot
    }

    /// Checks the decoded manifest `raw`, and returns the manifest.
    fn from_raw(raw: RawManifest) -> Result<Self, Problem> {
        // The version and the magic were checked before the manifest was decoded.
        if u64::from(raw.saved_state_schema_version) != contract::DEVICE_STATE_SCHEMA_VERSION
            || raw.saved_state_root_type != SAVED_STATE_ROOT_TYPE
        {
            return Err(Problem::Format(format!(
                "has saved-state schema version {} of root type {:?}",
                raw.saved_state_schema_version, raw.saved_state_root_type
            )));
        }
        if raw.architecture != ARCHITECTURE || raw.page_size != PAGE_SIZE {
            return Err(invalid(format!(
                "describes a VM of architecture {:?} with {}-byte pages, not an x86-64 microVM",
                raw.architecture, raw.page_size
            )));
        }
        if !(1..=MAX_STATE_LEN).contains(&raw.state_size) {
            return Err(invalid(format!(
                "has a saved state of {} bytes, not 1 to {MAX_STATE_LEN}",
                raw.state_size
            )));
        }
        if (
            raw.tier.as_str(),
            raw.restore_policy.as_str(),
            raw.consumed_sections,
        ) != (TIER_PLATFORM, RESTORE_POLICY_CLONE, CONSUMED_INVARIANTS)
        {
            return Err(invalid(format!(
                "is not a template's: it was captured at tier {:?} with restore policy {:?} after \
                 consuming configuration sections {:#x}, not at tier \"platform\" for clones after \
                 consuming its invariants alone",
                raw.tier, raw.restore_policy, raw.consumed_sections
            )));
        }
        let (seconds, nanos) = raw.created_at;
        let created_at = u64::try_from(seconds)
            .ok()
            .filter(|_| (0..NANOS_PER_SECOND).contains(&nanos))
            .and_then(|seconds| {
                SystemTime::UNIX_EPOCH.checked_add(Duration::new(seconds, nanos.unsigned_abs()))
            })
            .ok_or_else(|| {
                invalid(format!(
                    "has the invalid creation time of {seconds} seconds and {nanos} nanoseconds"
                ))
            })?;
        let Some(contract) = raw.contract else {
            return Err(invalid("has no machine contract"));
        };
        let (machine, time) = check_contract(contract, raw.vp_count, raw.memory_size)?;
        Ok(Self {
            created_at,
            openvmm_version: raw.openvmm_version,
            state_len: raw.state_size,
            time,
            machine,
        })
    }
}

/// Why a manifest is refused: one of another format, or one that is not a template's.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Problem {
    /// The manifest is of a format that this build does not read, which this describes.
    Format(String),
    /// The manifest is malformed, or not a template's, for the reason that this gives.
    Invalid(String),
}

impl Problem {
    /// Returns the error of the manifest that `description` names.
    fn into_error(self, description: &str) -> Error {
        match self {
            Self::Format(problem) => Error::new(
                ErrorCode::FormatUnsupported,
                format!(
                    "{description} {problem}, which this build does not read; capture the template \
                     again with the runtime of this build"
                ),
            ),
            Self::Invalid(problem) => Error::io(format!("{description} {problem}")),
        }
    }
}

/// Returns an invalid manifest's [`Problem`], which `problem` describes.
fn invalid(problem: impl Into<String>) -> Problem {
    Problem::Invalid(problem.into())
}

/// Checks the format of the manifest `bytes`, its version and magic, from its top-level fields
/// alone, so that a manifest of another version, whose fields this build may not know, is reported
/// as such.
fn check_format(bytes: &[u8]) -> Result<(), Problem> {
    let all: Vec<u32> = (1..64).collect();
    let mut message = Message::new(bytes, &[], &all);
    let mut version = None;
    let mut magic = None;
    loop {
        let field = match message.next_field() {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => return Err(invalid(format!("is malformed: it {error}"))),
        };
        // A field that appears twice is refused once the manifest is decoded.
        match field.number {
            1 => version = field.uint64().ok(),
            12 => magic = field.bytes().ok(),
            _ => {}
        }
    }
    let version = version.unwrap_or(0);
    if version != contract::SNAPSHOT_FORMAT_VERSION {
        return Err(Problem::Format(format!("has format version {version}")));
    }
    if magic != Some(FORMAT_MAGIC) {
        return Err(Problem::Format(
            "has another format magic than its version's".to_owned(),
        ));
    }
    Ok(())
}

/// Decodes the fields of the message `data`, whose type knows the fields `known`, and `repeated`,
/// which may appear more than once, with `each`, and describes the first problem as one of `what`.
fn decode<'a>(
    data: &'a [u8],
    what: &str,
    known: &[u32],
    repeated: &[u32],
    mut each: impl FnMut(Field<'a>) -> Result<(), String>,
) -> Result<(), String> {
    let mut message = Message::new(data, known, repeated);
    loop {
        match message.next_field() {
            Ok(Some(field)) => each(field)?,
            Ok(None) => return Ok(()),
            Err(error) => return Err(format!("{what} {error}")),
        }
    }
}

/// Returns a mapper of a [`WireError`] of a field of the message `what`.
fn field_error(what: &str) -> impl Fn(WireError) -> String + '_ {
    move |error| format!("{what} {error}")
}

/// Appends `item` to `items`, which may hold at most `limit` items of the field that `what`
/// names.
fn push_bounded<T>(items: &mut Vec<T>, item: T, limit: usize, what: &str) -> Result<(), String> {
    if items.len() == limit {
        return Err(format!("{what} hold more than {limit} items"));
    }
    items.push(item);
    Ok(())
}

/// The manifest, as its fields encode it.
#[derive(Default)]
struct RawManifest {
    /// Field 2: seconds and nanoseconds.
    created_at: (i64, i32),
    /// Field 3.
    openvmm_version: String,
    /// Field 4.
    memory_size: u64,
    /// Field 5.
    vp_count: u32,
    /// Field 6.
    page_size: u32,
    /// Field 7.
    architecture: String,
    /// Field 8.
    state_size: u64,
    /// Field 11.
    contract: Option<RawContract>,
    /// Field 13.
    saved_state_schema_version: u32,
    /// Field 14.
    saved_state_root_type: String,
    /// Field 15.
    tier: String,
    /// Field 16.
    restore_policy: String,
    /// Field 17.
    consumed_sections: u32,
}

/// Decodes `openvmm.snapshot.SnapshotManifest`, whose version, field 1, and magic, field 12,
/// [`check_format`] checked.
fn decode_manifest(data: &[u8]) -> Result<RawManifest, String> {
    let what = "it";
    let wire = field_error(what);
    let mut raw = RawManifest::default();
    decode(
        data,
        what,
        &[1, 2, 3, 4, 5, 6, 7, 8, 11, 12, 13, 14, 15, 16, 17],
        &[],
        |field| {
            match field.number {
                1 => {
                    field.uint32().map_err(&wire)?;
                }
                2 => raw.created_at = decode_timestamp(field.bytes().map_err(&wire)?)?,
                3 => raw.openvmm_version = field.text().map_err(&wire)?.to_owned(),
                4 => raw.memory_size = field.uint64().map_err(&wire)?,
                5 => raw.vp_count = field.uint32().map_err(&wire)?,
                6 => raw.page_size = field.uint32().map_err(&wire)?,
                7 => raw.architecture = field.text().map_err(&wire)?.to_owned(),
                8 => raw.state_size = field.uint64().map_err(&wire)?,
                11 => raw.contract = Some(decode_contract(field.bytes().map_err(&wire)?)?),
                12 => {
                    field.bytes().map_err(&wire)?;
                }
                13 => raw.saved_state_schema_version = field.uint32().map_err(&wire)?,
                14 => raw.saved_state_root_type = field.text().map_err(&wire)?.to_owned(),
                15 => raw.tier = field.text().map_err(&wire)?.to_owned(),
                16 => raw.restore_policy = field.text().map_err(&wire)?.to_owned(),
                17 => raw.consumed_sections = field.uint32().map_err(&wire)?,
                _ => unreachable!("the message refuses unknown fields"),
            }
            Ok(())
        },
    )?;
    Ok(raw)
}

/// Decodes `google.protobuf.Timestamp` into its seconds and nanoseconds.
fn decode_timestamp(data: &[u8]) -> Result<(i64, i32), String> {
    let what = "its creation time";
    let wire = field_error(what);
    let mut timestamp = (0, 0);
    decode(data, what, &[1, 2], &[], |field| {
        match field.number {
            1 => timestamp.0 = field.int64().map_err(&wire)?,
            2 => timestamp.1 = field.int32().map_err(&wire)?,
            _ => unreachable!("the message refuses unknown fields"),
        }
        Ok(())
    })?;
    Ok(timestamp)
}

/// The machine contract, as its fields encode it.
#[derive(Default)]
struct RawContract {
    /// Field 1.
    machine_profile: String,
    /// Field 2.
    abi_version: u32,
    /// Field 3.
    source_hypervisor: String,
    /// Field 4.
    command_line: String,
    /// Field 5.
    command_line_digest: Vec<u8>,
    /// Field 6: start, length, and file offset.
    memory_ranges: Vec<(u64, u64, u64)>,
    /// Field 7.
    topology: Topology,
    /// Field 8: each device and its order.
    devices: Vec<(Device, u32)>,
    /// Field 9: how many names it holds.
    state_units: usize,
    /// Field 10: how many attachments it holds.
    attachments: usize,
    /// Field 16.
    boot_layout_version: u32,
    /// Field 18.
    network: Option<RawNetwork>,
    /// Field 19: whether it is present.
    filesystem: bool,
    /// Field 21.
    blocks: Vec<RawBlock>,
    /// Field 22.
    filesystem_slot_version: u32,
    /// Field 23.
    boot_online_vp_count: u32,
    /// Field 24.
    interrupt_mode: String,
    /// Field 25.
    status_page_start: u64,
    /// Field 26.
    status_page_length: u64,
    /// Field 27.
    expansion_version: u32,
    /// Field 28.
    memory_capacity: u64,
    /// Field 29.
    memory_block_size: u64,
    /// Field 30.
    expansion_ranges: Vec<MemoryRange>,
    /// Field 31.
    time: Option<RawTime>,
    /// Field 32.
    cpu_profile: Option<RawCpuProfile>,
}

/// Decodes `openvmm.snapshot.SnapshotMachineContract`.
fn decode_contract(data: &[u8]) -> Result<RawContract, String> {
    let what = "its machine contract";
    let wire = field_error(what);
    let mut raw = RawContract::default();
    decode(
        data,
        what,
        &[
            1, 2, 3, 4, 5, 7, 16, 18, 19, 22, 23, 24, 25, 26, 27, 28, 29, 31, 32,
        ],
        &[6, 8, 9, 10, 21, 30],
        |field| {
            match field.number {
                1 => raw.machine_profile = field.text().map_err(&wire)?.to_owned(),
                2 => raw.abi_version = field.uint32().map_err(&wire)?,
                3 => raw.source_hypervisor = field.text().map_err(&wire)?.to_owned(),
                4 => raw.command_line = field.text().map_err(&wire)?.to_owned(),
                5 => raw.command_line_digest = field.bytes().map_err(&wire)?.to_vec(),
                6 => {
                    let range = decode_memory_range(field.bytes().map_err(&wire)?)?;
                    push_bounded(
                        &mut raw.memory_ranges,
                        range,
                        MAX_MEMORY_RANGES,
                        "its memory ranges",
                    )?;
                }
                7 => raw.topology = decode_topology(field.bytes().map_err(&wire)?)?,
                8 => {
                    let index = raw.devices.len();
                    let device = decode_device(field.bytes().map_err(&wire)?, index)?;
                    push_bounded(&mut raw.devices, device, MAX_DEVICES, "its devices")?;
                }
                9 => {
                    field.text().map_err(&wire)?;
                    if raw.state_units == MAX_STATE_UNITS {
                        return Err(format!(
                            "its state units hold more than {MAX_STATE_UNITS} items"
                        ));
                    }
                    raw.state_units += 1;
                }
                10 => {
                    decode_attachment(field.bytes().map_err(&wire)?, raw.attachments)?;
                    if raw.attachments == MAX_ATTACHMENTS {
                        return Err(format!(
                            "its attachments hold more than {MAX_ATTACHMENTS} items"
                        ));
                    }
                    raw.attachments += 1;
                }
                16 => raw.boot_layout_version = field.uint32().map_err(&wire)?,
                18 => raw.network = Some(decode_network(field.bytes().map_err(&wire)?)?),
                19 => {
                    // A template's filesystem slot is dormant, so the policy of an attached
                    // filesystem is refused without its fields being read.
                    field.bytes().map_err(&wire)?;
                    raw.filesystem = true;
                }
                21 => {
                    let block = decode_block(field.bytes().map_err(&wire)?)?;
                    push_bounded(&mut raw.blocks, block, MAX_BLOCKS, "its sandbox blocks")?;
                }
                22 => raw.filesystem_slot_version = field.uint32().map_err(&wire)?,
                23 => raw.boot_online_vp_count = field.uint32().map_err(&wire)?,
                24 => raw.interrupt_mode = field.text().map_err(&wire)?.to_owned(),
                25 => raw.status_page_start = field.uint64().map_err(&wire)?,
                26 => raw.status_page_length = field.uint64().map_err(&wire)?,
                27 => raw.expansion_version = field.uint32().map_err(&wire)?,
                28 => raw.memory_capacity = field.uint64().map_err(&wire)?,
                29 => raw.memory_block_size = field.uint64().map_err(&wire)?,
                30 => {
                    let (start, length) = decode_expansion_range(field.bytes().map_err(&wire)?)?;
                    push_bounded(
                        &mut raw.expansion_ranges,
                        MemoryRange { start, length },
                        MAX_MEMORY_RANGES,
                        "its memory expansion ranges",
                    )?;
                }
                31 => raw.time = Some(decode_time(field.bytes().map_err(&wire)?)?),
                32 => raw.cpu_profile = Some(decode_cpu_profile(field.bytes().map_err(&wire)?)?),
                _ => unreachable!("the message refuses unknown fields"),
            }
            Ok(())
        },
    )?;
    Ok(raw)
}

/// Decodes `openvmm.snapshot.SnapshotMemoryRange` into its start, length, and file offset.
fn decode_memory_range(data: &[u8]) -> Result<(u64, u64, u64), String> {
    let what = "its memory range";
    let wire = field_error(what);
    let mut range = (0, 0, 0);
    decode(data, what, &[1, 2, 3], &[], |field| {
        let value = field.uint64().map_err(&wire)?;
        match field.number {
            1 => range.0 = value,
            2 => range.1 = value,
            3 => range.2 = value,
            _ => unreachable!("the message refuses unknown fields"),
        }
        Ok(())
    })?;
    Ok(range)
}

/// Decodes `openvmm.snapshot.SnapshotMemoryExpansionRange`, a range that a restore may add, into
/// its start and length: it has no file offset.
fn decode_expansion_range(data: &[u8]) -> Result<(u64, u64), String> {
    let what = "its memory expansion range";
    let wire = field_error(what);
    let mut range = (0, 0);
    decode(data, what, &[1, 2], &[], |field| {
        let value = field.uint64().map_err(&wire)?;
        match field.number {
            1 => range.0 = value,
            2 => range.1 = value,
            _ => unreachable!("the message refuses unknown fields"),
        }
        Ok(())
    })?;
    Ok(range)
}

/// Decodes `openvmm.snapshot.SnapshotProcessorTopology`.
fn decode_topology(data: &[u8]) -> Result<Topology, String> {
    let what = "its processor topology";
    let wire = field_error(what);
    let mut topology = Topology::default();
    decode(data, what, &[1, 2, 3, 4, 5], &[], |field| {
        match field.number {
            1 => topology.sockets = field.uint32().map_err(&wire)?,
            2 => topology.dies_per_socket = field.uint32().map_err(&wire)?,
            3 => topology.cores_per_die = field.uint32().map_err(&wire)?,
            4 => topology.threads_per_core = field.uint32().map_err(&wire)?,
            5 => topology.apic_ids = field.packed_uint32().map_err(&wire)?,
            _ => unreachable!("the message refuses unknown fields"),
        }
        Ok(())
    })?;
    Ok(topology)
}

/// Decodes `openvmm.snapshot.SnapshotDevice`, the device at `index` of the machine, and returns
/// it with its order.
fn decode_device(data: &[u8], index: usize) -> Result<(Device, u32), String> {
    let what = format!("its device {index}");
    let wire = field_error(&what);
    let mut device = Device {
        stable_id: String::new(),
        kind: String::new(),
        state_unit: String::new(),
        transport: String::new(),
        ranges: Vec::new(),
        irq: None,
        feature_banks: Vec::new(),
        queue_count: 0,
        queue_max_sizes: Vec::new(),
    };
    let mut order = 0;
    decode(data, &what, &[1, 2, 3, 4, 6, 7, 8, 9, 10], &[5], |field| {
        match field.number {
            1 => device.stable_id = field.text().map_err(&wire)?.to_owned(),
            2 => device.state_unit = field.text().map_err(&wire)?.to_owned(),
            3 => device.kind = field.text().map_err(&wire)?.to_owned(),
            4 => order = field.uint32().map_err(&wire)?,
            5 => {
                let range = decode_device_range(field.bytes().map_err(&wire)?, index)?;
                push_bounded(
                    &mut device.ranges,
                    range,
                    MAX_DEVICE_RANGES,
                    &format!("the ranges of {what}"),
                )?;
            }
            6 => device.irq = Some(field.uint32().map_err(&wire)?),
            7 => device.transport = field.text().map_err(&wire)?.to_owned(),
            8 => device.feature_banks = field.packed_uint32().map_err(&wire)?,
            9 => device.queue_count = field.uint32().map_err(&wire)?,
            10 => device.queue_max_sizes = field.packed_uint32().map_err(&wire)?,
            _ => unreachable!("the message refuses unknown fields"),
        }
        Ok(())
    })?;
    Ok((device, order))
}

/// Decodes `openvmm.snapshot.SnapshotDeviceRange`, a range of the device at `index`.
fn decode_device_range(data: &[u8], index: usize) -> Result<DeviceRange, String> {
    let what = format!("a range of its device {index}");
    let wire = field_error(&what);
    let mut space = String::new();
    let mut range = DeviceRange {
        space: AddressSpace::Mmio,
        start: 0,
        length: 0,
    };
    decode(data, &what, &[1, 2, 3], &[], |field| {
        match field.number {
            1 => space = field.text().map_err(&wire)?.to_owned(),
            2 => range.start = field.uint64().map_err(&wire)?,
            3 => range.length = field.uint64().map_err(&wire)?,
            _ => unreachable!("the message refuses unknown fields"),
        }
        Ok(())
    })?;
    range.space = AddressSpace::parse(&space)
        .ok_or_else(|| format!("{what} is in the unknown address space {space:?}"))?;
    Ok(range)
}

/// Decodes `openvmm.snapshot.SnapshotAttachment`, the host attachment at `index`, which a key
/// does not hold, so that only its form is checked.
fn decode_attachment(data: &[u8], index: usize) -> Result<(), String> {
    let what = format!("its attachment {index}");
    let wire = field_error(&what);
    decode(data, &what, &[1, 2, 3, 4, 5, 6, 7, 8], &[], |field| {
        match field.number {
            1 | 2 | 4 | 5 => {
                field.text().map_err(&wire)?;
            }
            3 => {
                field.boolean().map_err(&wire)?;
            }
            6 => {
                if field.bytes().map_err(&wire)?.len() > MAX_ATTACHMENT_IDENTITY_LEN {
                    return Err(format!(
                        "{what} has an identity of more than {MAX_ATTACHMENT_IDENTITY_LEN} bytes"
                    ));
                }
            }
            7 | 8 => {
                field.uint64().map_err(&wire)?;
            }
            _ => unreachable!("the message refuses unknown fields"),
        }
        Ok(())
    })
}

/// The network of the machine contract, as its fields encode it.
#[derive(Default)]
struct RawNetwork {
    /// Field 9.
    profile: String,
    /// Field 1.
    ipv4: u32,
    /// Field 2.
    prefix_length: u32,
    /// Field 3.
    gateway: u32,
    /// Field 4.
    guest_mac: Vec<u8>,
    /// Field 5.
    gateway_mac: Vec<u8>,
    /// Field 6.
    policy_mode: String,
    /// Field 7.
    policy_digest: Vec<u8>,
    /// Field 8.
    policy_required: bool,
    /// Field 10.
    policy_encoding_version: u32,
    /// Field 11.
    ipv6: Vec<u8>,
    /// Field 12.
    ipv6_prefix_length: u32,
    /// Field 13.
    ipv6_gateway: Vec<u8>,
}

/// Decodes `openvmm.snapshot.SnapshotMicrovmNetwork`.
fn decode_network(data: &[u8]) -> Result<RawNetwork, String> {
    let what = "its network";
    let wire = field_error(what);
    let mut raw = RawNetwork::default();
    decode(
        data,
        what,
        &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13],
        &[],
        |field| {
            match field.number {
                1 => raw.ipv4 = field.uint32().map_err(&wire)?,
                2 => raw.prefix_length = field.uint32().map_err(&wire)?,
                3 => raw.gateway = field.uint32().map_err(&wire)?,
                4 => raw.guest_mac = field.bytes().map_err(&wire)?.to_vec(),
                5 => raw.gateway_mac = field.bytes().map_err(&wire)?.to_vec(),
                6 => raw.policy_mode = field.text().map_err(&wire)?.to_owned(),
                7 => raw.policy_digest = field.bytes().map_err(&wire)?.to_vec(),
                8 => raw.policy_required = field.boolean().map_err(&wire)?,
                9 => raw.profile = field.text().map_err(&wire)?.to_owned(),
                10 => raw.policy_encoding_version = field.uint32().map_err(&wire)?,
                11 => raw.ipv6 = field.bytes().map_err(&wire)?.to_vec(),
                12 => raw.ipv6_prefix_length = field.uint32().map_err(&wire)?,
                13 => raw.ipv6_gateway = field.bytes().map_err(&wire)?.to_vec(),
                _ => unreachable!("the message refuses unknown fields"),
            }
            Ok(())
        },
    )?;
    Ok(raw)
}

/// A sandbox block of the machine contract, as its fields encode it.
#[derive(Default)]
struct RawBlock {
    /// Field 1.
    role: String,
    /// Field 2.
    read_only: bool,
    /// Fields 3, 7, and 8.
    geometry: (u64, u32, u32),
    /// Field 4.
    identity_kind: String,
    /// Field 5.
    identity: Vec<u8>,
    /// Field 6.
    artifact: String,
    /// Field 9.
    restore_mode: String,
}

/// Decodes `openvmm.snapshot.SnapshotMicrovmSandboxBlock`.
fn decode_block(data: &[u8]) -> Result<RawBlock, String> {
    let what = "its sandbox block";
    let wire = field_error(what);
    let mut raw = RawBlock::default();
    decode(data, what, &[1, 2, 3, 4, 5, 6, 7, 8, 9], &[], |field| {
        match field.number {
            1 => raw.role = field.text().map_err(&wire)?.to_owned(),
            2 => raw.read_only = field.boolean().map_err(&wire)?,
            3 => raw.geometry.0 = field.uint64().map_err(&wire)?,
            4 => raw.identity_kind = field.text().map_err(&wire)?.to_owned(),
            5 => raw.identity = field.bytes().map_err(&wire)?.to_vec(),
            6 => raw.artifact = field.text().map_err(&wire)?.to_owned(),
            7 => raw.geometry.1 = field.uint32().map_err(&wire)?,
            8 => raw.geometry.2 = field.uint32().map_err(&wire)?,
            9 => raw.restore_mode = field.text().map_err(&wire)?.to_owned(),
            _ => unreachable!("the message refuses unknown fields"),
        }
        Ok(())
    })?;
    Ok(raw)
}

/// The time contract, as its fields encode it.
#[derive(Default)]
struct RawTime {
    /// Field 1.
    abi_version: u32,
    /// Field 2.
    tsc_hz: u64,
    /// Field 3.
    tolerance_ppm: u32,
    /// Field 4.
    apic_hz: u64,
    /// Field 5.
    capture_tsc: u64,
    /// Field 6.
    capture_utc_ns: u64,
    /// Field 7.
    capture_monotonic_ns: u64,
    /// Field 8.
    host_clock: String,
    /// Field 9.
    host_id: Vec<u8>,
    /// Field 10.
    boot_id: Vec<u8>,
    /// Field 11.
    lineage: u32,
    /// Field 12.
    cut_to_anchor_ns: Option<u64>,
}

/// Decodes `openvmm.snapshot.SnapshotTimeContract`.
fn decode_time(data: &[u8]) -> Result<RawTime, String> {
    let what = "its time contract";
    let wire = field_error(what);
    let mut raw = RawTime::default();
    decode(
        data,
        what,
        &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
        &[],
        |field| {
            match field.number {
                1 => raw.abi_version = field.uint32().map_err(&wire)?,
                2 => raw.tsc_hz = field.uint64().map_err(&wire)?,
                3 => raw.tolerance_ppm = field.uint32().map_err(&wire)?,
                4 => raw.apic_hz = field.uint64().map_err(&wire)?,
                5 => raw.capture_tsc = field.uint64().map_err(&wire)?,
                6 => raw.capture_utc_ns = field.uint64().map_err(&wire)?,
                7 => raw.capture_monotonic_ns = field.uint64().map_err(&wire)?,
                8 => raw.host_clock = field.text().map_err(&wire)?.to_owned(),
                9 => raw.host_id = field.bytes().map_err(&wire)?.to_vec(),
                10 => raw.boot_id = field.bytes().map_err(&wire)?.to_vec(),
                11 => raw.lineage = field.uint32().map_err(&wire)?,
                12 => raw.cut_to_anchor_ns = Some(field.uint64().map_err(&wire)?),
                _ => unreachable!("the message refuses unknown fields"),
            }
            Ok(())
        },
    )?;
    Ok(raw)
}

/// The CPU profile record, as its fields encode it.
#[derive(Default)]
struct RawCpuProfile {
    /// Field 1.
    id: String,
    /// Field 2.
    digest: Vec<u8>,
    /// Field 4.
    effective_cpuid: Vec<u8>,
}

/// Decodes `openvmm.snapshot.SnapshotCpuProfile`. Its canonical document, field 3, which its
/// digest names, and the capturing host's CPU signature, field 6, which is diagnostic, are not
/// kept.
fn decode_cpu_profile(data: &[u8]) -> Result<RawCpuProfile, String> {
    let what = "its CPU profile";
    let wire = field_error(what);
    let mut raw = RawCpuProfile::default();
    decode(data, what, &[1, 2, 3, 4, 6], &[], |field| {
        match field.number {
            1 => raw.id = field.text().map_err(&wire)?.to_owned(),
            2 => raw.digest = field.bytes().map_err(&wire)?.to_vec(),
            3 => {
                field.bytes().map_err(&wire)?;
            }
            4 => raw.effective_cpuid = field.bytes().map_err(&wire)?.to_vec(),
            6 => {
                field.uint32().map_err(&wire)?;
            }
            _ => unreachable!("the message refuses unknown fields"),
        }
        Ok(())
    })?;
    Ok(raw)
}

/// Checks the decoded machine contract `raw` of a manifest that records `vp_count` processors and
/// `memory_size` bytes of RAM, and returns the machine and the time contract.
fn check_contract(
    raw: RawContract,
    vp_count: u32,
    memory_size: u64,
) -> Result<(Machine, TimeContract), Problem> {
    if raw.machine_profile != MACHINE_PROFILE
        || u64::from(raw.abi_version) != contract::MICROVM_ABI_VERSION
        || raw.boot_layout_version != BOOT_LAYOUT_VERSION
    {
        return Err(Problem::Format(format!(
            "describes a machine of profile {:?}, ABI version {}, and boot layout version {}",
            raw.machine_profile, raw.abi_version, raw.boot_layout_version
        )));
    }
    let time = check_time(raw.time)?;
    if raw.expansion_version != MEMORY_EXPANSION_VERSION {
        return Err(invalid(format!(
            "is not a template's: it declares memory expansion version {}, not version 1",
            raw.expansion_version
        )));
    }
    if raw.filesystem_slot_version != FILESYSTEM_SLOT_VERSION || raw.filesystem {
        return Err(invalid(
            "is not a template's: its filesystem slot is not the dormant slot of version 1",
        ));
    }
    let hypervisor = raw.source_hypervisor.parse::<Hypervisor>().map_err(|_| {
        invalid(format!(
            "was captured by the unknown backend {:?}",
            raw.source_hypervisor
        ))
    })?;
    let cpu = check_cpu_profile(raw.cpu_profile)?;
    let command_line_digest = <[u8; SHA256_LEN]>::try_from(raw.command_line_digest.as_slice())
        .map_err(|_| invalid("has an effective command line whose digest is not 32 bytes"))?;
    // The RAM at boot fills the memory file from its start, range after range.
    let mut offset = 0u64;
    let mut ranges = Vec::with_capacity(raw.memory_ranges.len());
    for &(start, length, file_offset) in &raw.memory_ranges {
        if file_offset != offset {
            return Err(invalid(
                "has memory ranges that do not fill its memory file in order",
            ));
        }
        offset = offset.saturating_add(length);
        ranges.push(MemoryRange { start, length });
    }
    let (distro, scratch) = check_blocks(raw.blocks)?;
    let nic = raw.network.map(check_network).transpose()?;
    let machine = Machine {
        hypervisor,
        cpu,
        topology: raw.topology,
        rates: Rates {
            tsc_hz: time.tsc_frequency_hz,
            apic_hz: time.apic_frequency_hz,
            tolerance_ppm: time.tsc_tolerance_ppm,
        },
        processors: vp_count,
        boot_processors: raw.boot_online_vp_count,
        memory: Memory {
            boot: memory_size,
            capacity: raw.memory_capacity,
            block_size: raw.memory_block_size,
            ranges,
            expansion: raw.expansion_ranges,
        },
        command_line: raw.command_line,
        command_line_digest,
        distro,
        scratch,
        interrupts: Interrupts {
            mode: raw.interrupt_mode,
            status_page_start: raw.status_page_start,
            status_page_length: raw.status_page_length,
        },
        devices: check_order(raw.devices)?,
        filesystem_slot: raw.filesystem_slot_version,
        nic,
    };
    if let Some(problem) = machine.problem() {
        return Err(invalid(problem));
    }
    Ok((machine, time))
}

/// Checks the decoded time contract `raw`, and returns it.
fn check_time(raw: Option<RawTime>) -> Result<TimeContract, Problem> {
    let Some(raw) = raw else {
        return Err(invalid("has no time contract"));
    };
    if u64::from(raw.abi_version) != contract::TIME_ABI_VERSION {
        return Err(Problem::Format(format!(
            "has a time contract of time ABI version {}",
            raw.abi_version
        )));
    }
    let host_clock = HostClock::parse(&raw.host_clock).ok_or_else(|| {
        invalid(format!(
            "has a time contract of the unknown host clock {:?}",
            raw.host_clock
        ))
    })?;
    let identity = |bytes: &[u8], what: &str| {
        <[u8; HOST_IDENTITY_LEN]>::try_from(bytes).map_err(|_| {
            invalid(format!(
                "has a time contract whose {what} is {} bytes, not {HOST_IDENTITY_LEN}",
                bytes.len()
            ))
        })
    };
    Ok(TimeContract {
        tsc_frequency_hz: raw.tsc_hz,
        tsc_tolerance_ppm: raw.tolerance_ppm,
        apic_frequency_hz: raw.apic_hz,
        capture_tsc: raw.capture_tsc,
        capture_utc_ns: raw.capture_utc_ns,
        capture_monotonic_ns: raw.capture_monotonic_ns,
        host_clock,
        host_id: identity(&raw.host_id, "host identity")?,
        boot_id: BootId::from_bytes(identity(&raw.boot_id, "boot identity")?),
        lineage: raw.lineage,
        cut_to_anchor_ns: raw.cut_to_anchor_ns,
    })
}

/// Checks the decoded CPU profile record `raw`, and returns the profile.
fn check_cpu_profile(raw: Option<RawCpuProfile>) -> Result<CpuProfile, Problem> {
    let Some(raw) = raw else {
        return Err(invalid("has no CPU profile"));
    };
    let (vendor, generation, revision) = parse_profile_id(&raw.id).ok_or_else(|| {
        invalid(format!(
            "has the CPU profile {:?}, which is not VENDOR.GENERATION.vREVISION",
            raw.id
        ))
    })?;
    let digest = <[u8; SHA256_LEN]>::try_from(raw.digest.as_slice())
        .map_err(|_| invalid("has a CPU profile whose digest is not 32 bytes"))?;
    Ok(CpuProfile {
        vendor,
        generation,
        revision,
        digest,
        effective_cpuid: raw.effective_cpuid,
    })
}

/// Checks that the decoded devices `raw` are in their order, and returns them.
fn check_order(raw: Vec<(Device, u32)>) -> Result<Vec<Device>, Problem> {
    raw.into_iter()
        .enumerate()
        .map(|(index, (device, order))| {
            if order as usize == index {
                Ok(device)
            } else {
                Err(invalid(format!(
                    "lists device {index} with the order {order}"
                )))
            }
        })
        .collect()
}

/// Checks the decoded sandbox blocks `raw`: a template's read-only `distro`, whose identity is
/// unbound, then its fresh scratch. Returns their geometries, which [`Machine::problem`] checks.
fn check_blocks(raw: Vec<RawBlock>) -> Result<(BlockGeometry, BlockGeometry), Problem> {
    let not_template = || {
        invalid(
            "is not a template's: its sandbox blocks are not a read-only distro whose identity is \
             unbound and a fresh scratch",
        )
    };
    let [distro, scratch]: [RawBlock; 2] = raw.try_into().map_err(|_| not_template())?;
    let is = |block: &RawBlock, role: &str, read_only: bool, identity_kind: &str| {
        block.role == role
            && block.read_only == read_only
            && block.identity_kind == identity_kind
            && block.identity.is_empty()
            && block.artifact.is_empty()
            && block.restore_mode.is_empty()
    };
    if !is(&distro, "distro", true, "unbound") || !is(&scratch, "scratch", false, "fresh") {
        return Err(not_template());
    }
    let geometry = |block: &RawBlock| BlockGeometry {
        length: block.geometry.0,
        logical_block_size: block.geometry.1,
        physical_block_size: block.geometry.2,
    };
    Ok((geometry(&distro), geometry(&scratch)))
}

/// Checks the decoded network `raw`, and returns the fixed NIC, which [`Machine::problem`]
/// checks.
fn check_network(raw: RawNetwork) -> Result<Nic, Problem> {
    let mac = |bytes: &[u8]| <[u8; 6]>::try_from(bytes).ok();
    let (Some(guest_mac), Some(gateway_mac)) = (mac(&raw.guest_mac), mac(&raw.gateway_mac)) else {
        return Err(invalid("has a NIC whose MAC addresses are not 6 bytes"));
    };
    let prefix_length = u8::try_from(raw.prefix_length)
        .map_err(|_| invalid("has a NIC with an invalid prefix length"))?;
    let ipv6 = match (
        raw.ipv6.as_slice(),
        raw.ipv6_prefix_length,
        raw.ipv6_gateway.as_slice(),
    ) {
        ([], 0, []) => None,
        (address, prefix_length, gateway) => {
            let (Ok(address), Ok(prefix_length), Ok(gateway)) = (
                <[u8; 16]>::try_from(address),
                u8::try_from(prefix_length),
                <[u8; 16]>::try_from(gateway),
            ) else {
                return Err(invalid("has a NIC with an invalid IPv6 identity"));
            };
            Some(Ipv6Identity {
                address: Ipv6Addr::from(address),
                prefix_length,
                gateway: Ipv6Addr::from(gateway),
            })
        }
    };
    let digest = <[u8; SHA256_LEN]>::try_from(raw.policy_digest.as_slice())
        .map_err(|_| invalid("has a NIC whose policy digest is not 32 bytes"))?;
    Ok(Nic {
        profile: raw.profile,
        guest_mac,
        gateway_mac,
        ipv4: Ipv4Addr::from(raw.ipv4),
        prefix_length,
        gateway: Ipv4Addr::from(raw.gateway),
        ipv6,
        policy: PolicyIdentity {
            mode: raw.policy_mode,
            digest,
            required: raw.policy_required,
            encoding_version: raw.policy_encoding_version,
        },
    })
}

#[cfg(test)]
mod tests;
