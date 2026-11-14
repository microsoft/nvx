//! What every launch shares: a [`Runtime`], the OpenVMM executable, guest kernel, and guest
//! initramfs that it runs, with their [`RuntimeIdentity`], the [`Hypervisor`], which
//! [`probe_hypervisor`] checks, and the [`LaunchOptions`].
//!
//! The runtime identity of a VM is the identities of its VMM, kernel, and initramfs, and the
//! compatibility of the contracts through which its caller drives its guest, which the caller
//! names. A caller saves it with each VM, in a [`SavedVm`](crate::host::vm::SavedVm), which
//! [`vm::verify`](crate::host::vm::verify) checks against the current one.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use super::platform::hypervisor::Hypervisor;

use super::platform;
use crate::error::{Error, Result};

/// What every launch shares: the runtime's files and their identity, the hypervisor, and the
/// options of OpenVMM that do not depend on the VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runtime {
    /// The OpenVMM executable.
    pub openvmm: PathBuf,
    /// The guest kernel.
    pub kernel: PathBuf,
    /// The guest initramfs.
    pub initramfs: PathBuf,
    /// The identity of the three files, which the caller takes from their install or staging
    /// metadata, and of the contracts through which the caller drives their guest: NVX compares
    /// identities and never reads the files.
    pub identity: RuntimeIdentity,
    /// The hypervisor that OpenVMM runs the VM on.
    pub hypervisor: Hypervisor,
    /// The CPU profile that OpenVMM presents to the guest, by OpenVMM's name for it, or `None`
    /// for OpenVMM's default.
    pub cpu_profile: Option<String>,
    /// Windows only: start OpenVMM outside the caller's job object, so that the VM survives the
    /// caller. The job must permit breakaway.
    pub windows_breakaway: bool,
    /// The options of OpenVMM that every launch shares.
    pub options: LaunchOptions,
}

/// The options of OpenVMM that every launch shares. The boot console is not one of them: each VM
/// attaches its own.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LaunchOptions {
    /// What OpenVMM does on a plain guest power-off or hibernate event. A guest power-off that
    /// reports an exit status always exits OpenVMM with that status, regardless of this action.
    pub guest_shutdown: GuestPowerAction,
    /// How long OpenVMM waits for the host to authenticate on the control console, or `None` for
    /// OpenVMM's default. The cold-boot renderer accepts whole milliseconds from 1 to 60000.
    pub control_auth_timeout: Option<Duration>,
}

/// What OpenVMM does on a plain guest power-off or hibernate event. A power-off with an exit
/// status always exits OpenVMM with that status instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum GuestPowerAction {
    /// OpenVMM exits, which ends the VM: the default.
    #[default]
    Exit,
    /// OpenVMM halts the VM and keeps running, unless the power-off reports an exit status.
    Halt,
    /// OpenVMM restarts the guest, unless the power-off reports an exit status.
    Reset,
}

/// Checks that `hypervisor` is present and accessible on this host: its device on Linux, and the
/// Windows Hypervisor Platform on Windows.
///
/// Fails with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) otherwise.
pub fn probe_hypervisor(hypervisor: Hypervisor) -> Result<()> {
    platform::probe_hypervisor(hypervisor).map_err(Error::unavailable)
}

/// Longest [`ArtifactIdentity`] and [`CompatibilityIdentity`], in bytes.
pub const MAX_IDENTITY_LEN: usize = 128;

/// Hexadecimal digits of a SHA-256 digest: two for each of its 32 bytes.
const SHA256_HEX_LEN: usize = 64;

/// Opaque text of 1 to [`MAX_IDENTITY_LEN`] printable ASCII characters and no space: the form of
/// an [`ArtifactIdentity`] and of a [`CompatibilityIdentity`], which the library only compares.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Token(String);

impl Token {
    /// Wraps `text`, which `what` names, if it has the form of a token.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) otherwise.
    fn new(text: String, what: &str) -> Result<Self> {
        if (1..=MAX_IDENTITY_LEN).contains(&text.len())
            && text.bytes().all(|byte| byte.is_ascii_graphic())
        {
            Ok(Self(text))
        } else {
            Err(Error::unsupported(format!(
                "{what} {text:?} must hold 1 to {MAX_IDENTITY_LEN} printable ASCII characters and \
                 no space"
            )))
        }
    }
}

/// Identity of one runtime artifact, such as the OpenVMM executable, as the metadata of its
/// install or staging records it: for example `sha256:` and the hexadecimal SHA-256 digest that
/// a release's `SHA256SUMS` lists for the file.
///
/// It is an opaque token of 1 to [`MAX_IDENTITY_LEN`] printable ASCII characters without spaces,
/// which the library only compares. Nothing here reads or hashes an artifact: a digest computed at
/// launch would cost time in proportion to the artifact's size on every start.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ArtifactIdentity(Token);

impl ArtifactIdentity {
    /// Wraps `identity`.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) unless it holds 1 to
    /// [`MAX_IDENTITY_LEN`] printable ASCII characters and no space.
    pub fn new(identity: impl Into<String>) -> Result<Self> {
        Token::new(identity.into(), "artifact identity").map(Self)
    }

    /// Returns the identity `sha256:DIGEST` of an artifact whose SHA-256 digest is `digest`, 64
    /// hexadecimal digits in either case, which the identity holds in lowercase.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) otherwise.
    pub fn sha256(digest: &str) -> Result<Self> {
        if digest.len() == SHA256_HEX_LEN && digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            Self::new(format!("sha256:{}", digest.to_ascii_lowercase()))
        } else {
            Err(Error::unsupported(format!(
                "{digest:?} is not a SHA-256 digest of {SHA256_HEX_LEN} hexadecimal digits"
            )))
        }
    }

    /// Returns the identity's text.
    pub fn as_str(&self) -> &str {
        &self.0.0
    }
}

impl fmt::Display for ArtifactIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl TryFrom<String> for ArtifactIdentity {
    type Error = Error;

    fn try_from(identity: String) -> Result<Self> {
        Self::new(identity)
    }
}

impl From<ArtifactIdentity> for String {
    fn from(identity: ArtifactIdentity) -> Self {
        identity.0.0
    }
}

/// Identity of the contracts through which a caller drives the guests of a runtime, as the caller
/// names them: such as the boot tokens through which it configures a guest, the root that the
/// guest assembles from them, and the control protocol that the guest's agent serves.
///
/// The library declares no contract here: it implements none of its caller's, and only compares
/// the identity. Each caller names the contracts of its own guests and control plane, and changes
/// the identity whenever it could not drive a VM that a runtime of the earlier identity started
/// as one that it starts. Like an [`ArtifactIdentity`], it is an opaque token of 1 to
/// [`MAX_IDENTITY_LEN`] printable ASCII characters without spaces.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CompatibilityIdentity(Token);

impl CompatibilityIdentity {
    /// Wraps `identity`.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) unless it holds 1 to
    /// [`MAX_IDENTITY_LEN`] printable ASCII characters and no space.
    pub fn new(identity: impl Into<String>) -> Result<Self> {
        Token::new(identity.into(), "compatibility identity").map(Self)
    }

    /// Returns the identity's text.
    pub fn as_str(&self) -> &str {
        &self.0.0
    }
}

impl fmt::Display for CompatibilityIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl TryFrom<String> for CompatibilityIdentity {
    type Error = Error;

    fn try_from(identity: String) -> Result<Self> {
        Self::new(identity)
    }
}

impl From<CompatibilityIdentity> for String {
    fn from(identity: CompatibilityIdentity) -> Self {
        identity.0.0
    }
}

/// A part of a [`RuntimeIdentity`], which [`RuntimeIdentity::differences`] names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IdentityPart {
    /// [`RuntimeIdentity::openvmm`].
    OpenVmm,
    /// [`RuntimeIdentity::kernel`].
    Kernel,
    /// [`RuntimeIdentity::initramfs`].
    Initramfs,
    /// [`RuntimeIdentity::compatibility`].
    Compatibility,
}

impl IdentityPart {
    /// Every part, in the order of the fields of [`RuntimeIdentity`].
    pub const ALL: [Self; 4] = [
        Self::OpenVmm,
        Self::Kernel,
        Self::Initramfs,
        Self::Compatibility,
    ];

    /// Returns the part's name, as messages print it.
    pub const fn name(self) -> &'static str {
        match self {
            Self::OpenVmm => "OpenVMM",
            Self::Kernel => "kernel",
            Self::Initramfs => "initramfs",
            Self::Compatibility => "compatibility",
        }
    }
}

impl fmt::Display for IdentityPart {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// Identity of the runtime that a VM was launched with: its OpenVMM executable, guest kernel, and
/// guest initramfs, and the compatibility of the contracts through which its caller drives its
/// guest.
///
/// It decides whether a host may keep managing a VM that it finds recorded, as after an
/// orchestrator restart or an upgrade of the orchestrator. Under the same runtime identity, a new
/// build of the host's own code keeps the VM running and reattaches to it. A different runtime
/// identity would drive the VM through artifacts or contracts that did not start it, so the VM
/// must be stopped instead, through its verified process identity; see
/// [`vm::verify`](crate::host::vm::verify).
///
/// The caller supplies every part: the artifact identities from install or staging metadata, and
/// the [`CompatibilityIdentity`] of its own guests and control plane. The library adds nothing:
/// it neither knows nor declares which contracts the artifacts implement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeIdentity {
    /// The OpenVMM executable.
    pub openvmm: ArtifactIdentity,
    /// The guest kernel.
    pub kernel: ArtifactIdentity,
    /// The guest initramfs.
    pub initramfs: ArtifactIdentity,
    /// The compatibility of the contracts through which the caller drives the guest.
    pub compatibility: CompatibilityIdentity,
}

impl RuntimeIdentity {
    /// Returns the identity of a runtime of these artifacts, which the caller drives through the
    /// contracts that `compatibility` names.
    pub fn new(
        openvmm: ArtifactIdentity,
        kernel: ArtifactIdentity,
        initramfs: ArtifactIdentity,
        compatibility: CompatibilityIdentity,
    ) -> Self {
        Self {
            openvmm,
            kernel,
            initramfs,
            compatibility,
        }
    }

    /// Returns the parts of this identity that differ from `other`, in the order of its fields.
    pub fn differences(&self, other: &Self) -> Vec<IdentityPart> {
        IdentityPart::ALL
            .into_iter()
            .filter(|&part| !self.same_part(other, part))
            .collect()
    }

    /// Returns whether this identity and `other` agree in `part`.
    fn same_part(&self, other: &Self, part: IdentityPart) -> bool {
        match part {
            IdentityPart::OpenVmm => self.openvmm == other.openvmm,
            IdentityPart::Kernel => self.kernel == other.kernel,
            IdentityPart::Initramfs => self.initramfs == other.initramfs,
            IdentityPart::Compatibility => self.compatibility == other.compatibility,
        }
    }
}

#[cfg(test)]
mod tests;
