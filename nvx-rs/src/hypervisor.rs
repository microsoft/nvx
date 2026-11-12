//! The hypervisors that run NVX microVMs.

use std::fmt;
use std::str::FromStr;

use crate::error::{Error, Result};

/// Hypervisor that runs the VMs.
///
/// With the `serde` feature, it serializes as its OpenVMM spelling: `kvm`, `mshv`, or `whp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "lowercase")
)]
pub enum Hypervisor {
    /// Linux KVM through `/dev/kvm`.
    Kvm,
    /// Linux Microsoft Hypervisor through `/dev/mshv`.
    Mshv,
    /// Windows Hypervisor Platform.
    Whp,
}

impl Hypervisor {
    /// Environment variable that overrides [`Hypervisor::from_env_or_default`]: `kvm`, `mshv`, or
    /// `whp`.
    pub const ENV: &'static str = "NVX_HYPERVISOR";

    /// Returns the OpenVMM spelling of this hypervisor.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Kvm => "kvm",
            Self::Mshv => "mshv",
            Self::Whp => "whp",
        }
    }

    /// Returns the hypervisor named by [`Hypervisor::ENV`], or the
    /// [platform default](Self::platform_default) when the variable is unset.
    ///
    /// Fails with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) when the variable
    /// names an unknown hypervisor or the host has no default.
    pub fn from_env_or_default() -> Result<Self> {
        match std::env::var(Self::ENV) {
            Ok(value) if !value.is_empty() => value.parse(),
            Ok(_) | Err(std::env::VarError::NotPresent) => {
                Self::platform_default().ok_or_else(|| {
                    Error::unavailable(format!(
                        "this host has no default hypervisor; set {}",
                        Self::ENV
                    ))
                })
            }
            Err(error) => Err(
                Error::unavailable(format!("{} is not valid Unicode", Self::ENV))
                    .with_source(error),
            ),
        }
    }

    /// Returns the conventional hypervisor for this host: KVM on Linux and WHP on Windows.
    pub fn platform_default() -> Option<Self> {
        if cfg!(target_os = "linux") {
            Some(Self::Kvm)
        } else if cfg!(windows) {
            Some(Self::Whp)
        } else {
            None
        }
    }

    /// Returns whether this hypervisor can exist on the current host's operating system.
    pub fn supported_on_host(self) -> bool {
        match self {
            Self::Kvm | Self::Mshv => cfg!(target_os = "linux"),
            Self::Whp => cfg!(windows),
        }
    }
}

impl fmt::Display for Hypervisor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Hypervisor {
    type Err = Error;

    /// Parses the OpenVMM spelling of a hypervisor, failing with
    /// [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) for any other.
    fn from_str(value: &str) -> Result<Self> {
        match value {
            "kvm" => Ok(Self::Kvm),
            "mshv" => Ok(Self::Mshv),
            "whp" => Ok(Self::Whp),
            _ => Err(Error::unavailable(format!(
                "unknown hypervisor {value:?}; choose kvm, mshv, or whp"
            ))),
        }
    }
}

#[cfg(test)]
mod tests;
