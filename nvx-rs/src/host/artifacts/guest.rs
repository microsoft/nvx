//! The Linux guests that `nvx.py` builds and runs, and the names of their artifacts.

use std::fmt;

use super::{DISTRO_MANIFEST_SUFFIX, PACKAGE_MANIFEST_SUFFIX};
use crate::error::{Error, Result};

/// A Linux guest that `nvx.py` builds and runs, as `scripts/nvx_tools/guests.py` describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Guest {
    /// Alpine Linux: the default guest of `nvx-rs run`, and the one that runs sandboxes.
    Alpine,
    /// Ubuntu Base, which also provides the distro layer of sandbox workloads.
    Ubuntu,
    /// Azure Linux.
    AzureLinux,
}

impl Guest {
    /// Every guest, in the order in which `nvx.py` lists them.
    pub const ALL: [Self; 3] = [Self::Alpine, Self::Ubuntu, Self::AzureLinux];

    /// Returns the guest's name in `nvx.py` options: `alpine`, `ubuntu`, or `azurelinux`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Alpine => "alpine",
            Self::Ubuntu => "ubuntu",
            Self::AzureLinux => "azurelinux",
        }
    }

    /// Returns the guest named `name`.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) and the message
    /// `unsupported guest "NAME"; choose alpine, ubuntu, azurelinux`, which quotes the name with
    /// Rust's debug formatting.
    pub fn from_name(name: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|guest| guest.name() == name)
            .ok_or_else(|| {
                let names: Vec<&str> = Self::ALL.iter().map(|guest| guest.name()).collect();
                Error::unsupported(format!(
                    "unsupported guest {name:?}; choose {}",
                    names.join(", ")
                ))
            })
    }

    /// Returns the name of the guest's distribution, as messages show it: `Alpine Linux`,
    /// `Ubuntu Base`, or `Azure Linux`.
    pub const fn distribution(self) -> &'static str {
        match self {
            Self::Alpine => "Alpine Linux",
            Self::Ubuntu => "Ubuntu Base",
            Self::AzureLinux => "Azure Linux",
        }
    }

    /// Returns the file name of the guest's initramfs.
    pub const fn initramfs_name(self) -> &'static str {
        match self {
            Self::Alpine => "initramfs.cpio.gz",
            Self::Ubuntu => "initramfs-ubuntu.cpio.gz",
            Self::AzureLinux => "initramfs-azurelinux.cpio.gz",
        }
    }

    /// Returns the RAM, in MiB, with which `nvx-rs run` boots the guest without `--memory-mib`:
    /// the `default_memory_mib` of its descriptor in `scripts/nvx_tools/guests.py`, which
    /// `nvx.py test-microvm` also uses, and which a test of `nvx.py` checks against the help of
    /// `nvx-rs run`.
    pub const fn default_memory_mib(self) -> u32 {
        match self {
            Self::Alpine => 128,
            Self::Ubuntu | Self::AzureLinux => 512,
        }
    }

    /// Returns the file name of the package manifest of the guest's initramfs.
    pub fn package_manifest_name(self) -> String {
        format!("{}{PACKAGE_MANIFEST_SUFFIX}", self.initramfs_name())
    }

    /// Returns the file name of the guest's EROFS distro layer, `GUEST-distro.erofs`, if the
    /// guest provides one: only Ubuntu does.
    pub const fn distro_layer_name(self) -> Option<&'static str> {
        match self {
            Self::Ubuntu => Some("ubuntu-distro.erofs"),
            Self::Alpine | Self::AzureLinux => None,
        }
    }

    /// Returns the file name of the manifest of the guest's distro layer, if it has one.
    pub fn distro_manifest_name(self) -> Option<String> {
        self.distro_layer_name()
            .map(|name| format!("{name}{DISTRO_MANIFEST_SUFFIX}"))
    }

    /// Returns the file name of the guest's sandbox scratch template, the empty ext4 file system
    /// of which the sandbox scenarios of `nvx.py` boot a copy as their scratch, if the guest
    /// provides one: only Ubuntu does, beside its distro layer.
    #[cfg(feature = "cli")]
    pub(crate) const fn scratch_template_name(self) -> Option<&'static str> {
        match self {
            Self::Ubuntu => Some("ubuntu-smoke-scratch.ext4"),
            Self::Alpine | Self::AzureLinux => None,
        }
    }
}

impl fmt::Display for Guest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}
