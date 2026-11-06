//! The configuration of a VM, as one value that [`VmConfig::check`] checks without reading the
//! host: the VM's processors and memory, its [`RootConfig`], the [`View`] that its workload has
//! of it, its [`NetworkConfig`], the host paths that it exports, its [`ConsoleAttachment`], and
//! what only its cold boot takes, a [`Boot`].

use std::ops::RangeInclusive;
use std::path::PathBuf;

use super::filesystem::{ExportPlan, ReservedTargets};
use super::network::NetworkPolicy;
use crate::activation::{self, Journal, MAX_HOSTNAME_BYTES, OverlayMode, View};
use crate::error::{Error, Result};
use crate::net::Ipv4Cidr;
use crate::protocol::root::{ImageFormat, RootImage, RootLayout, RootUpper};
use crate::protocol::view;

/// The guest address of a VM's NIC at a cold boot whose configuration names none, in the form
/// that [`Ipv4Cidr::parse`] reads.
pub const DEFAULT_GUEST_NETWORK: &str = "10.0.0.2/24";

/// The caps of a RAM upper that [`VmConfig::check`] accepts, in percent of the VM's memory.
const RAM_UPPER_CAPS: RangeInclusive<u8> = 1..=90;

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
}

impl VmConfig {
    /// Renders a cold boot's kernel command line without reading the host: the root's tokens,
    /// the view's tokens, the export's mapping count when present, then the caller's
    /// [`Boot::command_line`] without trimming or rewriting it.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) when
    /// [`check`](Self::check) refuses the configuration or when it has no [`Boot`]. A restore
    /// receives its view through its activation record, not a kernel command line.
    pub fn kernel_command_line(&self) -> Result<String> {
        self.check()?;
        let boot = self
            .boot
            .as_ref()
            .ok_or_else(|| Error::unsupported("a kernel command line needs a cold boot's Boot"))?;
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

    /// Checks the whole configuration without reading the host: it opens no path, and checks no
    /// support of the host.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported), naming the first
    /// refusal in the order of the fields: no processor; no memory; a layered root without an image
    /// path, with a bound of an EROFS image's first partition or a bound of 0, with a scratch upper
    /// without a path, or with a RAM upper whose cap is not 1 to 90 percent; a hostname that is
    /// empty, longer than [`MAX_HOSTNAME_BYTES`], or holds a NUL; and an export that
    /// [`ExportPlan::check`] refuses for the root, whose error is the source.
    ///
    /// The NIC's address is checked when it is built, and the network's policy where it is
    /// lowered, with the guest's address. This check does not inspect [`Boot::command_line`],
    /// which [`kernel_command_line`](Self::kernel_command_line) appends unchanged.
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

#[cfg(test)]
mod tests;
