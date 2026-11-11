//! Explicit guest artifact ISA, independent of the host's hypervisor backend.

/// The ISA of a guest artifact set. Existing constructors retain the x86-64 default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Architecture {
    /// Existing x86-64 artifact paths and formats.
    #[default]
    X86_64,
    /// ARM64 cold-boot `Image` and architecture-qualified guest artifacts.
    Aarch64,
}

impl Architecture {
    /// Selects the native ISA for runtime commands, not the legacy producer/API default.
    #[cfg(feature = "cli")]
    pub(crate) const fn native() -> Self {
        if cfg!(target_arch = "aarch64") {
            Self::Aarch64
        } else {
            Self::X86_64
        }
    }

    /// Returns the canonical build/manifest architecture token.
    pub const fn name(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        }
    }

    /// Returns the kernel filename in this architecture's existing producer output.
    pub const fn kernel_name(self) -> &'static str {
        match self {
            Self::X86_64 => super::KERNEL_NAME,
            Self::Aarch64 => "Image",
        }
    }

    /// Returns the corresponding kernel provenance filename.
    pub(super) const fn kernel_provenance_name(self) -> &'static str {
        match self {
            Self::X86_64 => super::KERNEL_PROVENANCE_NAME,
            Self::Aarch64 => "Image.provenance.json",
        }
    }
}

#[cfg(test)]
mod tests;
