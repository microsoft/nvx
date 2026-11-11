//! The configuration of a VM, as one value that [`VmConfig::check`] checks without reading the
//! host: the VM's processors and memory, its [`RootConfig`], the [`View`] that its workload has
//! of it, its [`NetworkConfig`], the host paths that it exports, its [`ConsoleAttachment`], and
//! what only its cold boot takes, a [`Boot`], with what a capture of the VM adds, a
//! [`CaptureConfig`], whose [`CaptureIdentity`] names the capture's snapshots.
//! An internal renderer builds a whole cold launch for [`crate::host::vm::create`],
//! from that configuration and a
//! [`Runtime`], without opening runtime paths or allocating endpoints.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::filesystem::{ExportPlan, ReservedTargets};
use super::network::{self, NetworkPolicy};
use super::platform::parse_hex32;
use super::runtime::{GuestPowerAction, Runtime};
use crate::activation::{self, Journal, MAX_HOSTNAME_BYTES, OverlayMode, View};
use crate::contract;
use crate::error::{Error, Result};
use crate::hypervisor::Hypervisor;
use crate::net::Ipv4Cidr;
use crate::protocol::cmdline::{self, Parameter};
use crate::protocol::maps::HostMap;
use crate::protocol::root::{ImageFormat, RootImage, RootLayout, RootUpper};
use crate::protocol::view;

/// Bytes of the x86 kernel command line available to caller tokens, including the NUL.
pub const COMMAND_LINE_BUDGET: usize = 1024;

/// Longest Windows process command line, in UTF-16 code units, including the terminating NUL.
///
/// OpenVMM's command line is held to it on every host, so a policy behaves alike on all of them.
pub const PROCESS_COMMAND_LINE_LIMIT: usize = 32_767;

/// Room for the runtime's files and the VM's output paths, as legacy adapter planning reserves.
pub(crate) const OPENVMM_COMMAND_LINE_RESERVE: usize = 2048;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct NicConfig {
    /// The NIC's static address.
    pub guest_network: Ipv4Cidr,
}

impl NetworkConfig {
    /// Resolves the address that a cold launch attaches, shared by preparation and rendering.
    pub(crate) fn cold_nic(&self) -> Result<NicConfig> {
        Ok(NicConfig {
            guest_network: match self.guest_network {
                Some(address) => address,
                None => Ipv4Cidr::parse(DEFAULT_GUEST_NETWORK).map_err(Error::unsupported)?,
            },
        })
    }
}

/// What only a VM's cold boot takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boot {
    /// The caller's own kernel command-line tokens, to which NVX adds those of what it lowers.
    /// The root's tokens, `nvx_hostname`, `nvx_machine_id`, `nvx_maps` and `virtfs_*` are reserved;
    /// every other token, including `nvx_entrypoint`, `nvx_arg` and `nvx_share`, is caller-owned.
    /// A capture permits only OpenVMM's platform allowlist and refuses caller tokens whose
    /// tier, processor, console, device or network settings the builder supplies. It requires
    /// exactly one `nvx_sandbox=1` token, as the guest's init splits the command line.
    pub command_line: String,
    /// Memory capacity of a cold VM, in MiB, if it reserves more than its boot memory.
    /// It must be at least [`VmConfig::memory_mib`] and fit a byte count.
    pub memory_capacity_mib: Option<u64>,
    /// What a capture of the VM adds, if the VM may be captured.
    pub capture: Option<CaptureConfig>,
}

/// What a capture adds to the configuration of the VM that it captures: how the snapshot treats
/// the disks, the processors and the memory that the VM boots with and that a restore may grow
/// it to, and the caller's invariant settings. The capture VM's own configuration gives the
/// rest, such as its NIC, its console, and its export.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Capacity {
    /// The resource at boot.
    pub boot: u64,
    /// The capacity: at least the resource at boot.
    pub capacity: u64,
}

/// The length and block sizes of a sandbox block device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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

impl serde::Serialize for Invariants {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(&self.0, serializer)
    }
}

impl<'de> serde::Deserialize<'de> for Invariants {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let settings = <BTreeMap<String, String> as serde::Deserialize>::deserialize(deserializer)?;
        settings
            .into_iter()
            .try_fold(Self::new(), |settings, (name, value)| {
                settings.with(&name, &value)
            })
            .map_err(serde::de::Error::custom)
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
    /// Returns the configured export's mapping table, in exactly the order and form of
    /// [`ExportPlan::guest_table`], or an empty table without an export.
    ///
    /// This is a pure projection: it reads no host path and does not recheck the plan. A cold
    /// boot's rendered `nvx_maps` token counts these entries.
    pub fn mapping_table(&self) -> Vec<HostMap> {
        self.filesystem
            .as_ref()
            .map_or_else(Vec::new, ExportPlan::guest_table)
    }

    /// Renders a cold boot's kernel command line without reading the host: the root's tokens,
    /// the view's tokens, the export's mapping count when present, then the caller's
    /// [`Boot::command_line`] without trimming or rewriting it.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) when
    /// [`check`](Self::check) refuses the configuration or when it has no [`Boot`]. A restore
    /// receives its view through its activation record, not a kernel command line.
    /// A capture renders only accepted platform tokens and its boot-online processor count;
    /// OpenVMM adds the single tier token from the capture's `--snapshot-tier platform` option.
    pub fn kernel_command_line(&self) -> Result<String> {
        self.check()?;
        let boot = self
            .boot
            .as_ref()
            .ok_or_else(|| Error::unsupported("a kernel command line needs a cold boot's Boot"))?;
        if let Some(capture) = &boot.capture {
            return self.capture_command_line(boot, capture);
        }
        let mut tokens = Vec::new();
        if let RootConfig::Layered(root) = &self.root {
            let (upper, journal, max_memory_percent) = match &root.upper {
                UpperConfig::Scratch { journal, .. } => (RootUpper::Scratch, Some(*journal), None),
                UpperConfig::Ram { max_memory_percent } => {
                    (RootUpper::Ram, None, Some(*max_memory_percent))
                }
            };
            tokens.extend(
                RootLayout {
                    image: root.format,
                    upper,
                    overlay: Some(root.overlay),
                    journal,
                    gpt_first_partition_bound: root.gpt_first_partition_bound,
                    max_memory_percent,
                }
                .tokens(),
            );
        }
        tokens.extend(
            view::tokens(&self.view)
                .map_err(|error| Error::unsupported(error.to_string()).with_source(error))?,
        );
        if let Some(export) = &self.filesystem {
            tokens.push(export.kernel_token());
        }
        if !boot.command_line.is_empty() {
            tokens.push(boot.command_line.clone());
        }
        Ok(tokens.join(" "))
    }

    /// Renders a blockless capture without root, view, mapping or workload tokens.
    fn capture_command_line(&self, boot: &Boot, capture: &CaptureConfig) -> Result<String> {
        if self.root != RootConfig::Blockless {
            return Err(Error::unsupported("a fresh capture needs a blockless root"));
        }
        if self.filesystem.is_some() {
            return Err(Error::unsupported(
                "a fresh capture refuses any filesystem export",
            ));
        }
        if !capture.memory.boot.is_multiple_of(MEMORY_BLOCK_MIB) {
            return Err(Error::unsupported(format!(
                "a fresh capture's boot memory of {} MiB is not a multiple of {MEMORY_BLOCK_MIB} MiB",
                capture.memory.boot
            )));
        }
        for token in boot.command_line.split_ascii_whitespace() {
            let name = token.split_once('=').map_or(token, |(name, _)| name);
            if !platform_command_line_token_is_invariant(token)
                || [
                    "earlycon",
                    "console",
                    "virtio_mmio.device",
                    "virtnet_ip",
                    "virtnet_mask",
                    "virtnet_gw",
                    "virtnet_dns",
                    "virtnet_ip6",
                    "virtnet_gw6",
                    "nvx_snapshot_tier",
                    "nr_cpus",
                    "maxcpus",
                    "nvx_control_tty",
                ]
                .contains(&name)
            {
                return Err(Error::unsupported(format!(
                    "Boot.command_line uses the forbidden platform capture token {name:?}"
                )));
            }
        }
        let mut sandbox = cmdline::init_tokens(boot.command_line.as_bytes())
            .filter(|token| token.name == b"nvx_sandbox");
        if sandbox.next().and_then(|token| token.value) != Some(b"1") || sandbox.next().is_some() {
            return Err(Error::unsupported(
                "a fresh capture needs exactly one nvx_sandbox=1 token",
            ));
        }
        let mut command_line = boot.command_line.clone();
        if capture.processors.boot < capture.processors.capacity {
            if !command_line.is_empty() {
                command_line.push(' ');
            }
            command_line.push_str(&format!("maxcpus={}", capture.processors.boot));
        }
        Ok(command_line)
    }

    /// Renders a cold boot's OpenVMM arguments from this configuration and
    /// `runtime`.
    ///
    /// `endpoint`, `report` and `console_endpoint` are already chosen output names, not launch
    /// settings: this method creates no directory, file, listener, or process, and uses paths as
    /// given. A [`ConsoleAttachment::Listener`] needs a nonempty `console_endpoint`; a
    /// [`ConsoleAttachment::Disconnected`] needs none and retains the console device with a
    /// discarded attachment. The control endpoint always listens and authenticates from stdin.
    /// Listener names must be distinct, with ASCII-case-insensitive comparison for WHP, and
    /// contain neither commas nor equals signs, which delimit OpenVMM's serial options.
    /// The caller runs `runtime.openvmm` with these arguments.
    ///
    /// [`crate::host::vm::create`] calls this internal whole-config renderer. Neither managed
    /// lifecycle nor workload-identity flags are rendered; the caller owns its guest tokens.
    /// `runtime.windows_breakaway` is process-launch behavior, not an OpenVMM argument.
    /// A capture's resources and tier are rendered here; creation adds its checked block paths
    /// and snapshot destination, then rechecks the complete command line.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a refused
    /// configuration, no [`Boot`], empty or NUL-containing required names
    /// or runtime paths, serial-option delimiters in listener names, duplicate listener names,
    /// non-Unicode export child roots, block paths that are not Unicode, hold
    /// disk-option delimiters or
    /// unbalanced brackets, or select a non-raw OpenVMM backend, an invalid authentication timeout,
    /// kernel text with double quotes or the standalone `--` delimiter, or a complete command line
    /// beyond the Windows limit.
    /// The complete limit check runs on every host, without a reserve.
    pub(crate) fn openvmm_arguments(
        &self,
        runtime: &Runtime,
        endpoint: &str,
        report: &Path,
        console_endpoint: Option<&str>,
    ) -> Result<Vec<OsString>> {
        let command_line = self.control_kernel_command_line()?;
        check_endpoint(endpoint, "endpoint")?;
        for (name, path) in [
            ("Runtime.openvmm", runtime.openvmm.as_path()),
            ("Runtime.kernel", runtime.kernel.as_path()),
            ("Runtime.initramfs", runtime.initramfs.as_path()),
            ("report", report),
        ] {
            if path.as_os_str().is_empty() || path.as_os_str().to_string_lossy().contains('\0') {
                return Err(Error::unsupported(format!(
                    "{name} must name a path without a NUL"
                )));
            }
        }
        let console = self.console_backend(runtime, endpoint, console_endpoint)?;
        let mut arguments = self.device_arguments()?;
        let capture = self.boot.as_ref().and_then(|boot| boot.capture.as_ref());
        arguments.extend(boot_arguments(
            runtime.hypervisor,
            capture.map_or(self.memory_mib, |capture| capture.memory.boot),
            &runtime.kernel,
            &runtime.initramfs,
            &command_line,
        ));
        if let Some(capture) = capture {
            arguments.extend(capture.openvmm_arguments());
        }
        arguments.extend(console_arguments(&console, endpoint, report));
        arguments.extend(runtime_arguments(runtime)?);
        check_argument_nuls(&arguments)?;
        fit_process_command_line(runtime.openvmm.as_os_str(), &arguments, 0)?;
        Ok(arguments)
    }

    /// Renders the console attachment shared by cold boots and restores.
    fn console_backend(
        &self,
        runtime: &Runtime,
        endpoint: &str,
        console_endpoint: Option<&str>,
    ) -> Result<String> {
        match (self.console, console_endpoint) {
            (ConsoleAttachment::Listener, Some(console_endpoint)) => {
                check_endpoint(console_endpoint, "console_endpoint")?;
                if console_endpoint == endpoint
                    || (runtime.hypervisor == Hypervisor::Whp
                        && console_endpoint.eq_ignore_ascii_case(endpoint))
                {
                    return Err(Error::unsupported(
                        "boot and control console listeners require distinct endpoints",
                    ));
                }
                Ok(format!("listen={console_endpoint}"))
            }
            (ConsoleAttachment::Disconnected, None) => Ok("none".to_owned()),
            (ConsoleAttachment::Listener, None) => Err(Error::unsupported(
                "a console listener needs console_endpoint",
            )),
            (ConsoleAttachment::Disconnected, Some(_)) => Err(Error::unsupported(
                "a disconnected console takes no console_endpoint",
            )),
        }
    }

    /// Checks the configuration's part of a cold boot's OpenVMM command line on every
    /// host, without reading or changing the host, with 2048 UTF-16 code units reserved for the
    /// runtime's files, launch options, and the VM's output paths.
    ///
    /// Like legacy adapter planning, the reserve foresees runtime-file and output names of up to
    /// 260 code units each, Windows' `MAX_PATH`. It does not certify arbitrary runtime paths or
    /// CPU-profile names: [`crate::host::vm::create`] checks the complete command line with the
    /// actual runtime and output names, without a reserve.
    /// A listener's backend prefix is counted here; its endpoint name stays in the reserve.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a refused
    /// configuration or no [`Boot`], an unlowerable non-capture network policy, a
    /// NUL-containing argument, a non-Unicode export child root or block path, one that OpenVMM's
    /// disk syntax cannot hold literally, or one whose
    /// extension selects a non-raw backend, kernel text with double quotes or the standalone `--`
    /// delimiter, or insufficient room for the reserve.
    pub fn check_command_line(&self) -> Result<()> {
        let command_line = self.control_kernel_command_line()?;
        let mut arguments = self.device_arguments()?;
        let capture = self.boot.as_ref().and_then(|boot| boot.capture.as_ref());
        arguments.extend([
            "--memory".into(),
            format!(
                "{}M",
                capture.map_or(self.memory_mib, |capture| capture.memory.boot)
            )
            .into(),
            "--cmdline".into(),
            command_line.into(),
            "--virtio-console".into(),
            match self.console {
                ConsoleAttachment::Listener => "listen=",
                ConsoleAttachment::Disconnected => "none",
            }
            .into(),
        ]);
        if let Some(capture) = capture {
            arguments.extend(capture.openvmm_arguments());
        }
        check_argument_nuls(&arguments)?;
        fit_arguments(&arguments, OPENVMM_COMMAND_LINE_RESERVE)
    }

    /// Renders a boot's kernel text that OpenVMM's control console accepts.
    fn control_kernel_command_line(&self) -> Result<String> {
        let command_line = self.kernel_command_line()?;
        if command_line.contains('"')
            || command_line
                .split_ascii_whitespace()
                .any(|token| token == "--")
        {
            return Err(Error::unsupported(
                "boot.command_line cannot contain double quotes or a standalone -- with the \
                 OpenVMM control console",
            ));
        }
        Ok(command_line)
    }

    /// Renders only the configuration's devices; no runtime or output name is needed.
    fn device_arguments(&self) -> Result<Vec<OsString>> {
        let capture = self.boot.as_ref().and_then(|boot| boot.capture.as_ref());
        let mut arguments = vec![
            "--machine".into(),
            "microvm".into(),
            "--processors".into(),
            capture
                .map_or(u64::from(self.processors), |capture| {
                    capture.processors.capacity
                })
                .to_string()
                .into(),
        ];
        if let Some(capacity) = self.boot.as_ref().and_then(|boot| boot.memory_capacity_mib) {
            arguments.extend(["--memory-capacity".into(), format!("{capacity}M").into()]);
        }
        if let RootConfig::Layered(root) = &self.root {
            let scratch = match &root.upper {
                UpperConfig::Scratch { path, .. } => Some(path),
                UpperConfig::Ram { .. } => None,
            };
            for (path, writable) in [(Some(&root.image), false), (scratch, true)]
                .into_iter()
                .filter_map(|(path, writable)| path.map(|path| (path, writable)))
            {
                if path.to_str().is_none() {
                    return Err(Error::unsupported(
                        "block paths must be Unicode for OpenVMM's string arguments",
                    ));
                }
                let shown = path.display().to_string();
                reject_disk_path(path, &shown)?;
                reject_container_backend(path, &shown, writable, true)?;
            }
            let mut image = OsString::from("distro:file:");
            image.push(&root.image);
            image.push(",ro");
            arguments.extend(["--microvm-sandbox-block".into(), image]);
            if let UpperConfig::Scratch { path, .. } = &root.upper {
                let mut scratch = OsString::from("scratch:file:");
                scratch.push(path);
                arguments.extend(["--microvm-sandbox-block".into(), scratch]);
            }
        }
        if let Some(network) = &self.network {
            let guest_network = network.cold_nic()?.guest_network.to_string();
            let mut nic = if capture.is_some() {
                network::template_arguments(&guest_network)
            } else {
                network::openvmm_arguments(Some(&network.policy), &guest_network)?
            };
            if nic.is_empty() {
                // A configured NIC remains present even under the deny-all posture.
                nic = network::template_arguments(&guest_network);
            }
            arguments.extend(nic.into_iter().map(OsString::from));
        }
        if let Some(export) = &self.filesystem {
            if export
                .children
                .iter()
                .any(|child| child.root.to_str().is_none())
            {
                return Err(Error::unsupported(
                    "filesystem export child roots must be Unicode for OpenVMM's string arguments",
                ));
            }
            arguments.extend(export.openvmm_arguments().into_iter().map(OsString::from));
        }
        Ok(arguments)
    }

    /// Checks the whole configuration without reading the host: it opens no path, and checks no
    /// support of the host.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported), naming the first
    /// refusal in the order of the fields: no processor; no memory; a layered root without an image
    /// path, with a bound of an EROFS image's first partition or a bound of 0, with a scratch upper
    /// without a path, or with a RAM upper whose cap is not 1 to 90 percent; a hostname that is
    /// empty, longer than [`MAX_HOSTNAME_BYTES`], or holds a NUL; an export that
    /// [`ExportPlan::check`] refuses for the root, whose error is the source; a boot's capture
    /// that [`CaptureConfig::check`] refuses; and caller boot tokens that collide with the root's,
    /// the view's, `nvx_maps`, or any `virtfs_*` name.
    ///
    /// The NIC's address is checked when it is built, and the network's policy where it is
    /// lowered, with the guest's address. Boot tokens are read as both Linux and the guest's init
    /// read them, with dashes and underscores matching as Linux matches parameter names.
    /// [`kernel_command_line`](Self::kernel_command_line) appends accepted caller text unchanged.
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
        if let Some(boot) = &self.boot {
            if let Some(capacity) = boot.memory_capacity_mib
                && (capacity < self.memory_mib || capacity > u64::MAX >> 20)
            {
                return Err(Error::unsupported(
                    "a cold boot's memory capacity must be at least its memory and fit a byte count",
                ));
            }
            let guest = cmdline::init_tokens(boot.command_line.as_bytes()).map(|token| Parameter {
                name: token.name,
                value: token.value,
            });
            for parameter in cmdline::parameters(boot.command_line.as_bytes()).chain(guest) {
                if [
                    "nvx_layer",
                    "nvx_scratch",
                    "nvx_overlay_upper",
                    "nvx_overlay_mode",
                    "nvx_scratch_journal",
                    "nvx_first_partition_bound",
                    "nvx_ram_upper_percent",
                    "nvx_hostname",
                    "nvx_machine_id",
                    "nvx_maps",
                ]
                .iter()
                .any(|name| parameter.is_named(name))
                    || parameter.name.starts_with(b"virtfs_")
                    || parameter.name.starts_with(b"virtfs-")
                {
                    return Err(Error::unsupported(format!(
                        "Boot.command_line uses the reserved token {:?}",
                        String::from_utf8_lossy(parameter.name)
                    )));
                }
            }
        }
        Ok(())
    }
}

/// OpenVMM's named platform snapshot command-line allowlist.
fn platform_command_line_token_is_invariant(token: &str) -> bool {
    matches!(
        token,
        "earlycon=xe9"
            | "console=hvc0"
            | "console=hvc1"
            | "reboot=t"
            | "panic=-1"
            | "nvx_sandbox=1"
            | "nvx_config=0xd0010000,65536"
            | "nvx_snapshot_tier=platform"
            | "nvx_control_tty=hvc2"
    ) || token.starts_with("nr_cpus=")
        || token.starts_with("maxcpus=")
        || [
            "virtio_mmio.device=",
            "virtnet_ip=",
            "virtnet_mask=",
            "virtnet_gw=",
            "virtnet_dns=",
            "virtnet_ip6=",
            "virtnet_gw6=",
        ]
        .iter()
        .any(|prefix| token.starts_with(prefix))
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
    /// The capacity and platform tier shared by capture argument checks and rendering.
    fn openvmm_arguments(&self) -> [OsString; 4] {
        [
            "--memory-capacity".into(),
            format!("{}M", self.memory.capacity).into(),
            "--snapshot-tier".into(),
            "platform".into(),
        ]
    }

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

/// Refuses a disk path that OpenVMM's `--microvm-sandbox-block` value could not hold: one with
/// a comma, semicolon, or improperly nested brackets. The message shows `path` as `shown`.
pub(crate) fn reject_disk_path(path: &Path, shown: &str) -> Result<()> {
    let depth = path
        .to_string_lossy()
        .bytes()
        .try_fold(0_usize, |depth, byte| match byte {
            b'[' => Some(depth + 1),
            b']' => depth.checked_sub(1),
            b',' | b';' => None,
            _ => Some(depth),
        });
    if depth != Some(0) {
        return Err(Error::unsupported(format!(
            "disk paths containing commas, semicolons, or unbalanced brackets are unsupported: \
             {shown}"
        )));
    }
    Ok(())
}

/// Extensions for which every affected OpenVMM flow selects a container backend.
const CONTAINER_EXTENSIONS: [&str; 3] = ["vhd", "vhdx", "vmgs"];

/// Refuses a block path when OpenVMM would not attach it as the requested raw image.
fn reject_container_backend(
    path: &Path,
    shown: &str,
    writable: bool,
    reject_vmdk: bool,
) -> Result<()> {
    let name = path.file_name().map(|name| name.to_string_lossy());
    let extension = match name.as_deref().unwrap_or_default().rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => extension,
        _ => "",
    };
    if CONTAINER_EXTENSIONS.contains(&extension)
        || (reject_vmdk && extension == "vmdk")
        || (writable && extension == "iso")
    {
        return Err(Error::unsupported(format!(
            "disk paths ending in .{extension} are unsupported, since OpenVMM would not attach \
             them as raw images: {shown}"
        )));
    }
    Ok(())
}

/// Preserves the legacy sandbox refusal set used by snapshot and clone flows.
pub(crate) fn reject_container_path(path: &Path, shown: &str) -> Result<()> {
    reject_container_backend(path, shown, true, false)
}

/// Common cold-boot inputs, shared with the legacy VmmLaunch without narrowing its public types.
pub(crate) fn boot_arguments(
    hypervisor: Hypervisor,
    memory_mib: u64,
    kernel: &Path,
    initrd: &Path,
    kernel_command_line: &str,
) -> Vec<OsString> {
    let memory = format!("{memory_mib}M");
    [
        OsStr::new("--single-process"),
        OsStr::new("--hypervisor"),
        OsStr::new(hypervisor.as_str()),
        OsStr::new("--memory"),
        OsStr::new(&memory),
        OsStr::new("--kernel"),
        kernel.as_os_str(),
        OsStr::new("--initrd"),
        initrd.as_os_str(),
        OsStr::new("--cmdline"),
        OsStr::new(kernel_command_line),
    ]
    .map(OsStr::to_os_string)
    .into()
}

/// Common console, control authentication, and report arguments of a cold launch.
pub(crate) fn console_arguments(console: &str, endpoint: &str, report: &Path) -> Vec<OsString> {
    let control = format!("listen={endpoint}");
    [
        OsStr::new("--virtio-console"),
        OsStr::new(console),
        OsStr::new("--microvm-control-console"),
        OsStr::new(&control),
        OsStr::new("--microvm-control-auth-stdin"),
        OsStr::new("--microvm-report"),
        report.as_os_str(),
    ]
    .map(OsStr::to_os_string)
    .into()
}

/// Runtime options common to cold boots and restores.
fn runtime_arguments(runtime: &Runtime) -> Result<Vec<OsString>> {
    let mut arguments = Vec::new();
    if let Some(profile) = &runtime.cpu_profile {
        if profile.is_empty() {
            return Err(Error::unsupported("Runtime.cpu_profile must not be empty"));
        }
        arguments.extend(["--cpu-profile".into(), profile.as_str().into()]);
    }
    let shutdown = match runtime.options.guest_shutdown {
        GuestPowerAction::Exit => "exit",
        GuestPowerAction::Halt => "halt",
        GuestPowerAction::Reset => "reset",
    };
    arguments.extend(["--guest-shutdown-action".into(), shutdown.into()]);
    if let Some(timeout) = runtime.options.control_auth_timeout {
        if timeout < Duration::from_millis(1)
            || timeout > Duration::from_secs(60)
            || timeout.subsec_nanos() % 1_000_000 != 0
        {
            return Err(Error::unsupported(
                "LaunchOptions.control_auth_timeout must be whole milliseconds from 1 to 60000",
            ));
        }
        arguments.extend([
            "--microvm-control-auth-timeout-ms".into(),
            timeout.as_millis().to_string().into(),
        ]);
    }
    Ok(arguments)
}

/// Refuses caller-selected endpoint names that OpenVMM's serial options cannot hold literally.
fn check_endpoint(endpoint: &str, name: &str) -> Result<()> {
    if endpoint.is_empty() || endpoint.contains(['\0', ',', '=']) {
        return Err(Error::unsupported(format!(
            "{name} must name a nonempty endpoint without a NUL, comma, or equals sign",
        )));
    }
    Ok(())
}

/// Checks OpenVMM arguments for NULs that cannot be passed to a process.
fn check_argument_nuls(arguments: &[OsString]) -> Result<()> {
    if arguments
        .iter()
        .any(|argument| argument.to_string_lossy().contains('\0'))
    {
        return Err(Error::unsupported(
            "OpenVMM's arguments must not contain a NUL",
        ));
    }
    Ok(())
}

/// Returns the length in UTF-16 code units, including the terminating NUL, of the Windows command
/// line that runs `program` with `arguments`, quoted the way the Windows platform quotes it: the
/// program always in quotes, and each argument in quotes if it is empty or holds a space or a
/// tab, with a backslash before each quote that it holds and a backslash before each backslash
/// that precedes a quote, its closing quote included, and one space between them.
///
/// It is the length that [`PROCESS_COMMAND_LINE_LIMIT`] bounds, whatever the host:
/// [`fit_process_command_line`] checks it.
pub fn process_command_line_units(program: &OsStr, arguments: &[OsString]) -> usize {
    argument_units(program, true) + arguments_units(arguments)
}

/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) unless the Windows command
/// line that runs `program`, such as OpenVMM, with `arguments`, as [`process_command_line_units`]
/// counts it, leaves `reserve` code units of [`PROCESS_COMMAND_LINE_LIMIT`] unused: none for the
/// complete command line of a launch about to spawn OpenVMM, and some for what may change before
/// then, such as the paths of the artifacts.
///
/// The error says how long the command line is, and how to shorten it.
pub fn fit_process_command_line(
    program: &OsStr,
    arguments: &[OsString],
    reserve: usize,
) -> Result<()> {
    let length = process_command_line_units(program, arguments);
    let held = if reserve == 0 {
        String::new()
    } else {
        format!(", with {reserve} held in reserve")
    };
    fit("OpenVMM's command line", length, reserve, &held)
}

/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) unless OpenVMM's
/// `arguments`, which a plan renders, without the program and the rest of the launch, leave
/// `reserve` code units of [`PROCESS_COMMAND_LINE_LIMIT`] to that rest.
pub(crate) fn fit_arguments(arguments: &[OsString], reserve: usize) -> Result<()> {
    let held = format!(", with {reserve} held in reserve for the rest of the launch");
    fit(
        "OpenVMM's arguments",
        arguments_units(arguments),
        reserve,
        &held,
    )
}

/// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported), saying that `subject`
/// would take `length` code units, `held` describing `reserve`, unless they leave `reserve` of
/// [`PROCESS_COMMAND_LINE_LIMIT`] unused.
fn fit(subject: &str, length: usize, reserve: usize, held: &str) -> Result<()> {
    if length.saturating_add(reserve) <= PROCESS_COMMAND_LINE_LIMIT {
        return Ok(());
    }
    Err(Error::unsupported(format!(
        "{subject} would take {length} of the {PROCESS_COMMAND_LINE_LIMIT} characters that a \
         Windows process command line allows{held}; map, hide, or allow fewer or shorter \
         filesystem paths, or use fewer network rules"
    )))
}

/// Returns the length in UTF-16 code units of `arguments` on a Windows command line, quoted as
/// [`process_command_line_units`] quotes them, each with the space before it.
pub(crate) fn arguments_units(arguments: &[OsString]) -> usize {
    arguments
        .iter()
        .map(|argument| argument_units(argument, false))
        .sum()
}

/// Returns the length in UTF-16 code units of `argument` on a Windows command line, quoted as
/// [`process_command_line_units`] quotes it, always if it is the `program`, with one separating
/// space, or the terminating NUL after the last argument.
fn argument_units(argument: &OsStr, program: bool) -> usize {
    let text = argument.to_string_lossy();
    let quoted = program || text.is_empty() || text.contains([' ', '\t']);
    let mut length = 1 + text.encode_utf16().count() + if quoted { 2 } else { 0 };
    let mut backslashes = 0;
    for character in text.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                length += backslashes + 1;
                backslashes = 0;
            }
            _ => backslashes = 0,
        }
    }
    if quoted {
        length += backslashes;
    }
    length
}

#[cfg(test)]
mod tests;
