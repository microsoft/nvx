//! The explicitly selected guest instruction set, independent of the host.

use serde::{Deserialize, Serialize};

/// The instruction set of a runtime's guest kernel and programs.
///
/// The caller selects it from staging metadata. It is not inferred from the host, a filename,
/// a CPU-profile name, or an opaque runtime identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GuestArchitecture {
    /// The x86-64 instruction set.
    #[serde(rename = "x86_64")]
    X86_64,
    /// The AArch64 instruction set.
    #[serde(rename = "aarch64")]
    Aarch64,
}

impl GuestArchitecture {
    /// Returns the architecture's stable staging and command-line name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        }
    }
}
