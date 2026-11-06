//! The configuration of a VM, as one value that [`VmConfig::check`] checks without reading the
//! host: the VM's processors and memory, its [`RootConfig`], the [`View`] that its workload has
//! of it, its [`NetworkConfig`], the host paths that it exports, its [`ConsoleAttachment`], and
//! what only its cold boot takes, a [`Boot`], with what a capture of the VM adds, a
//! [`CaptureConfig`], whose [`CaptureIdentity`] names the capture's snapshots.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::RangeInclusive;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use super::filesystem::{ExportPlan, ReservedTargets};
use super::network::NetworkPolicy;
use super::platform::parse_hex32;
use super::runtime::Runtime;
use crate::activation::{self, Journal, MAX_HOSTNAME_BYTES, OverlayMode, View};
use crate::contract;
use crate::error::{Error, Result};
use crate::net::Ipv4Cidr;
use crate::protocol::root::{ImageFormat, RootImage};

/// The guest address of a VM's NIC at a cold boot whose configuration names none, in the form
/// that [`Ipv4Cidr::parse`] reads.
pub const DEFAULT_GUEST_NETWORK: &str = "10.0.0.2/24";

/// The caps of a RAM upper that [`VmConfig::check`] accepts, in percent of the VM's memory.
const RAM_UPPER_CAPS: RangeInclusive<u8> = 1..=90;

/// The processor counts of a microVM.
pub(crate) const PROCESSOR_COUNTS: [u32; 4] = [1, 2, 4, 8];

/// The size of the blocks in which a restore grows a VM's memory, in MiB, of which a capture's
/// memory capacity is a multiple.
const MEMORY_BLOCK_MIB: u64 = 128;

/// The domain of a capture's identity: the bytes that precede what identifies the capture in its
/// digest.
const CAPTURE_DOMAIN: &[u8] = b"nvx capture\0";

/// The configuration of a VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmConfig {
    /// The number of processors: at least 1.
    pub processors: u32,
    /// The memory, in MiB: at least 1.
    pub memory_mib: u64,
    /// The root that the guest runs.
    pub root: RootConfig,
    /// The view that the VM's workload has of it.
    pub view: View,
    /// The VM's network, if it has a NIC.
    pub network: Option<NetworkConfig>,
    /// The host paths that the VM exports to its guest, if any.
    pub filesystem: Option<ExportPlan>,
    /// Where the output of the VM's boot console goes.
    pub console: ConsoleAttachment,
    /// What only a cold boot takes: a VM that boots needs it, and one that a snapshot restores
    /// takes none.
    pub boot: Option<Boot>,
}

/// The root of a VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootConfig {
    /// The guest's own system, from its initramfs, without a block device.
    Blockless,
    /// A read-only image under an upper, which overlayfs combines.
    Layered(LayeredRoot),
}

/// A layered root: a read-only image under an upper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayeredRoot {
    /// The image, which OpenVMM opens by its path.
    pub image: PathBuf,
    /// The image's format, and the identity that the guest expects of the image, if any.
    pub format: RootImage,
    /// The most bytes that the first partition of a GPT disk may hold, if bounded: at least 1. An
    /// EROFS image has none.
    pub gpt_first_partition_bound: Option<u64>,
    /// Where the overlay keeps its upper.
    pub upper: UpperConfig,
    /// The overlay's mode.
    pub overlay: OverlayMode,
}

/// Where a layered root keeps the upper of its overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpperConfig {
    /// A whole-device ext4 filesystem on a scratch image.
    Scratch {
        /// The scratch image, which OpenVMM opens by its path.
        path: PathBuf,
        /// Whether the scratch's filesystem may have a journal.
        journal: Journal,
    },
    /// RAM, up to a cap.
    Ram {
        /// The most that the upper may hold, in percent of the VM's memory: 1 to 90.
        max_memory_percent: u8,
    },
}

impl UpperConfig {
    /// The cap of a RAM upper that a caller sets when it has no other: half of the VM's memory.
    pub const DEFAULT_MAX_MEMORY_PERCENT: u8 = 50;
}

/// Where the output of a VM's boot console goes. The console's device exists either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConsoleAttachment {
    /// OpenVMM listens for a client of the console.
    Listener,
    /// OpenVMM discards the console's output.
    Disconnected,
}

/// The network of a VM: the address of its NIC, and the policy that OpenVMM enforces on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkConfig {
    /// The NIC's static address, if the caller names one. Without one, a cold boot takes
    /// [`DEFAULT_GUEST_NETWORK`], and a restore the snapshot's.
    pub guest_network: Option<Ipv4Cidr>,
    /// The policy that OpenVMM enforces on the VM's traffic.
    pub policy: NetworkPolicy,
}

/// The NIC that a VM has, with the static address that it settled on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NicConfig {
    /// The NIC's static address.
    pub guest_network: Ipv4Cidr,
}

/// What only a VM's cold boot takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boot {
    /// The caller's own kernel command-line tokens, to which NVX adds those of what it lowers.
    pub command_line: String,
    /// What a capture of the VM adds, if the VM may be captured.
    pub capture: Option<CaptureConfig>,
}

/// What a capture adds to the configuration of the VM that it captures: how the snapshot treats
/// the disks, the processors and the memory that the VM boots with and that a restore may grow
/// it to, and the caller's invariant settings. The capture VM's own configuration gives the
/// rest, such as its NIC, its console, and its export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureConfig {
    /// How the snapshot treats the disks.
    pub disks: DiskCapture,
    /// The processors online at boot and their capacity: 1, 2, 4, or 8 each, and no more online
    /// at boot than the capacity.
    pub processors: Capacity,
    /// The memory at boot and its capacity, in MiB: some memory at boot, and a capacity of at
    /// least it, in multiples of 128 MiB.
    pub memory: Capacity,
    /// The caller's invariant settings, which NVX keeps without interpreting them.
    pub invariants: Invariants,
}

/// How a capture's snapshot treats the disks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DiskCapture {
    /// A fresh capture of a blockless VM, to which the capture attaches a placeholder of the
    /// image and a dormant scratch, which each restore replaces with its own.
    Fresh {
        /// The placeholder of the image: its length is the longest that a restore may attach.
        image: BlockGeometry,
        /// The dormant scratch.
        scratch: BlockGeometry,
    },
}

/// A resource at boot, and the capacity to which a restore may grow it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Capacity {
    /// The resource at boot.
    pub boot: u64,
    /// The capacity: at least the resource at boot.
    pub capacity: u64,
}

/// The length and block sizes of a sandbox block device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockGeometry {
    /// The length that the device presents, in bytes: nonzero, and a multiple of the logical
    /// block size.
    pub length: u64,
    /// The logical block size, in bytes: a power of two of at least 512.
    pub logical_block_size: u32,
    /// The physical block size, in bytes: a power of two of at least the logical block size.
    pub physical_block_size: u32,
}

impl BlockGeometry {
    /// Explains why the geometry is invalid, if it is, as a phrase whose subject is a size, such
    /// as "length 1000 is not a nonzero multiple of its logical block size".
    pub(crate) fn problem(&self) -> Option<String> {
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
    pub fn with(mut self, name: &str, value: &str) -> crate::Result<Self> {
        check_setting(name, value).map_err(Error::unsupported)?;
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
}

/// Explains why `name` and `value` are not an invariant setting, if they are not.
pub(crate) fn check_setting(name: &str, value: &str) -> Result<(), String> {
    let name_valid = (1..=Invariants::MAX_NAME_LEN).contains(&name.len())
        && name.starts_with(|first: char| first.is_ascii_lowercase())
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        });
    if !name_valid {
        return Err(format!(
            "invariant setting {name:?} must be named by 1 to {} lowercase ASCII letters, digits, \
             '.', '-', or '_', starting with a letter",
            Invariants::MAX_NAME_LEN
        ));
    }
    if value.len() > Invariants::MAX_VALUE_LEN
        || !value.bytes().all(|byte| (0x20..0x7f).contains(&byte))
    {
        return Err(format!(
            "the value of invariant setting {name:?} must hold at most {} printable ASCII \
             characters",
            Invariants::MAX_VALUE_LEN
        ));
    }
    Ok(())
}

/// The identity of a capture, which names its snapshots: the SHA-256 digest of what identifies
/// the capture, after the domain `nvx capture` and a NUL. [`CaptureConfig::identity`] returns it.
///
/// It displays as 64 lowercase hexadecimal digits, and serializes as that text.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CaptureIdentity([u8; 32]);

impl fmt::Display for CaptureIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0
            .iter()
            .try_for_each(|byte| write!(formatter, "{byte:02x}"))
    }
}

impl fmt::Debug for CaptureIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "CaptureIdentity({self})")
    }
}

impl serde::Serialize for CaptureIdentity {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for CaptureIdentity {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <String as serde::Deserialize>::deserialize(deserializer)?;
        parse_hex32(&text).map(Self).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "{text:?} is not a capture's identity, which is 64 hexadecimal digits"
            ))
        })
    }
}

impl VmConfig {
    /// Checks the whole configuration without reading the host: it opens no path, and checks no
    /// support of the host.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported), naming the first
    /// refusal in the order of the fields: no processor; no memory; a layered root without an image
    /// path, with a bound of an EROFS image's first partition or a bound of 0, with a scratch upper
    /// without a path, or with a RAM upper whose cap is not 1 to 90 percent; a hostname that is
    /// empty, longer than [`MAX_HOSTNAME_BYTES`], or holds a NUL; and an export that
    /// [`ExportPlan::check`] refuses for the root, whose error is the source. Last, it checks a
    /// boot's capture with [`CaptureConfig::check`], and fails with its error.
    ///
    /// The NIC's address is checked when it is built. The network's policy and the boot's command
    /// line are checked where they are lowered, with the guest's address and NVX's own tokens.
    pub fn check(&self) -> Result<()> {
        if self.processors == 0 {
            return Err(Error::unsupported("a VM needs at least one processor"));
        }
        if self.memory_mib == 0 {
            return Err(Error::unsupported("a VM needs memory"));
        }
        let reserved = match &self.root {
            RootConfig::Blockless => ReservedTargets::BLOCKLESS_ROOT,
            RootConfig::Layered(root) => {
                root.check()?;
                ReservedTargets::CONTAINER_ROOT
            }
        };
        if !activation::is_hostname(&self.view.hostname) {
            return Err(Error::unsupported(format!(
                "the hostname is not 1 to {MAX_HOSTNAME_BYTES} bytes without a NUL"
            )));
        }
        if let Some(plan) = &self.filesystem {
            plan.check(reserved).map_err(|error| {
                Error::unsupported("the export plan is invalid").with_source(error)
            })?;
        }
        if let Some(capture) = self.boot.as_ref().and_then(|boot| boot.capture.as_ref()) {
            capture.check()?;
        }
        Ok(())
    }
}

impl LayeredRoot {
    /// Checks the root's fields, as [`VmConfig::check`] describes.
    fn check(&self) -> Result<()> {
        if self.image.as_os_str().is_empty() {
            return Err(Error::unsupported("the layered root names no image"));
        }
        match (self.format.format, self.gpt_first_partition_bound) {
            (ImageFormat::Erofs, Some(_)) => {
                return Err(Error::unsupported(
                    "an EROFS image has no first partition to bound, as a GPT disk has",
                ));
            }
            (_, Some(0)) => {
                return Err(Error::unsupported(
                    "the bound of a GPT disk's first partition is 0, but a partition holds at \
                     least one block",
                ));
            }
            _ => {}
        }
        match &self.upper {
            UpperConfig::Scratch { path, .. } if path.as_os_str().is_empty() => Err(
                Error::unsupported("the scratch upper names no scratch image"),
            ),
            UpperConfig::Ram { max_memory_percent }
                if !RAM_UPPER_CAPS.contains(max_memory_percent) =>
            {
                Err(Error::unsupported(format!(
                    "the RAM upper's cap of {max_memory_percent}% of the VM's memory is not 1 to \
                     90%"
                )))
            }
            _ => Ok(()),
        }
    }
}

impl CaptureConfig {
    /// Checks the capture's settings without reading the host.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for processors online
    /// at boot or a processor capacity other than 1, 2, 4, or 8, more processors online at boot
    /// than the capacity, no memory at boot, a memory capacity below the memory at boot or not a
    /// multiple of 128 MiB, and a disk whose geometry is invalid.
    pub fn check(&self) -> Result<()> {
        let Capacity { boot, capacity } = self.processors;
        let supported =
            |count: u64| u32::try_from(count).is_ok_and(|count| PROCESSOR_COUNTS.contains(&count));
        if !supported(boot) || !supported(capacity) {
            return Err(Error::unsupported(format!(
                "a capture of {capacity} processors with {boot} online at boot needs 1, 2, 4, \
                 or 8 of each"
            )));
        }
        if boot > capacity {
            return Err(Error::unsupported(format!(
                "a capture of {capacity} processors can't have {boot} online at boot"
            )));
        }
        let Capacity { boot, capacity } = self.memory;
        if boot == 0 {
            return Err(Error::unsupported("a capture needs memory at boot"));
        }
        if capacity < boot {
            return Err(Error::unsupported(format!(
                "a capture's memory capacity of {capacity} MiB is below its {boot} MiB at boot"
            )));
        }
        if !capacity.is_multiple_of(MEMORY_BLOCK_MIB) {
            return Err(Error::unsupported(format!(
                "a capture's memory capacity of {capacity} MiB is not a multiple of \
                 {MEMORY_BLOCK_MIB} MiB"
            )));
        }
        match &self.disks {
            DiskCapture::Fresh { image, scratch } => {
                for (disk, geometry) in [("image", image), ("scratch", scratch)] {
                    if let Some(problem) = geometry.problem() {
                        return Err(Error::unsupported(format!(
                            "the {disk} disk of a capture is invalid: its {problem}"
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// Returns the identity of this capture of a VM of the configuration `config` on `runtime`,
    /// which names its snapshots: equal for equal inputs, and another when anything that tells
    /// captures apart differs.
    ///
    /// It identifies the captures that NVX supports, fresh captures of a blockless VM, from the
    /// values alone: it reads nothing on the host, refuses no configuration, which creating the
    /// capture checks, and digests nothing of the VM's root. It digests the runtime's identity,
    /// the versions of the contracts that this crate speaks, the hypervisor, and the CPU profile
    /// that the runtime requests; the capture's disks, processors, memory, and invariant
    /// settings; and of the VM, its NIC's address, which is [`DEFAULT_GUEST_NETWORK`] when the
    /// configuration names none, its console attachment, whether it exports host paths, and its
    /// boot's command line. It digests no path, nothing of a launch's options or of the view that
    /// each clone's activation sets, no network policy, which each restore replaces, and not the
    /// VM's own processors and memory, for which the capture's count.
    pub fn identity(&self, config: &VmConfig, runtime: &Runtime) -> CaptureIdentity {
        let mut fed = Identifying(Sha256::new_with_prefix(CAPTURE_DOMAIN));
        let identity = &runtime.identity;
        for artifact in [&identity.openvmm, &identity.kernel, &identity.initramfs] {
            fed.text(artifact.as_str());
        }
        fed.number(identity.boot_contract);
        fed.text(&identity.control_contract);
        fed.number(identity.control_session);
        for version in [
            contract::SNAPSHOT_FORMAT_VERSION,
            contract::DEVICE_STATE_SCHEMA_VERSION,
            contract::TIME_ABI_VERSION,
            contract::RESTORE_PACKET_VERSION,
            u64::from(activation::VERSION),
        ] {
            fed.number(version);
        }
        fed.text(runtime.hypervisor.as_str());
        fed.optional_text(runtime.cpu_profile.as_deref());
        match &self.disks {
            DiskCapture::Fresh { image, scratch } => {
                fed.text("fresh");
                fed.geometry(image);
                fed.geometry(scratch);
            }
        }
        for resource in [self.processors, self.memory] {
            fed.number(resource.boot);
            fed.number(resource.capacity);
        }
        fed.number(self.invariants.len() as u64);
        for (name, value) in self.invariants.iter() {
            fed.text(name);
            fed.text(value);
        }
        let address = config.network.as_ref().map(|network| {
            network.guest_network.map_or_else(
                || DEFAULT_GUEST_NETWORK.to_owned(),
                |address| address.to_string(),
            )
        });
        fed.optional_text(address.as_deref());
        fed.text(match config.console {
            ConsoleAttachment::Listener => "listener",
            ConsoleAttachment::Disconnected => "disconnected",
        });
        fed.number(u64::from(config.filesystem.is_some()));
        fed.optional_text(config.boot.as_ref().map(|boot| boot.command_line.as_str()));
        CaptureIdentity(fed.0.finalize().into())
    }
}

/// What identifies a capture, fed to its digest in a fixed order: each number in 8 bytes, and
/// each text after its length, so that no two sequences of values feed the same bytes.
struct Identifying(Sha256);

impl Identifying {
    /// Feeds a number.
    fn number(&mut self, number: u64) {
        self.0.update(number.to_be_bytes());
    }

    /// Feeds a text, after its length.
    fn text(&mut self, text: &str) {
        self.number(text.len() as u64);
        self.0.update(text.as_bytes());
    }

    /// Feeds whether there is a text, and the text if there is one.
    fn optional_text(&mut self, text: Option<&str>) {
        self.number(u64::from(text.is_some()));
        if let Some(text) = text {
            self.text(text);
        }
    }

    /// Feeds a geometry: its length and its block sizes.
    fn geometry(&mut self, geometry: &BlockGeometry) {
        self.number(geometry.length);
        self.number(u64::from(geometry.logical_block_size));
        self.number(u64::from(geometry.physical_block_size));
    }
}

#[cfg(test)]
mod tests;
