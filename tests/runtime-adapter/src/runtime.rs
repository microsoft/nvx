//! The runtime of the adapter's VMs: NVX's artifacts, whose initramfs boots the guest agent in
//! place of NVX's managed agent, and the runtime identity that NVX's supervisor records for a
//! launch and checks when it reattaches.
//!
//! The identity starts from the one that NVX gives its artifacts,
//! [`ArtifactDir::runtime_identity`], which names each artifact by the SHA-256 digest that the
//! install metadata beside it records, never from its contents: a release's `SHA256SUMS`, or the
//! build provenance in a repository checkout's `build/`, where `nvx.py download` also installs a
//! release's.
//!
//! The VMs boot a copy of NVX's initramfs that holds the guest agent; see [`crate::initramfs`].
//! Its identity names both: NVX's initramfs and the guest agent's build,
//! `sha256:DIGEST+runtime-adapter-guest/BUILD`. The guest agent has no install metadata of its
//! own, so the caller names its build, which is this crate's version,
//! [`GUEST_BUILD`](crate::GUEST_BUILD), unless the caller ships another. A host program that
//! ships another build of the guest agent runs another runtime, so it finds the VMs of this one
//! mismatched.

use std::path::Path;

use nvx::Result;
use nvx::host::artifacts::{ArtifactDir, Artifacts};
use nvx::host::store::{ArtifactIdentity, RuntimeIdentity};

/// NVX's artifacts, and the runtime identity of the adapter's VMs that boot them with the guest
/// agent.
#[derive(Debug, Clone)]
pub struct Runtime {
    /// The OpenVMM executable, the guest kernel, and NVX's initramfs, which the VMs boot a copy
    /// of, with the guest agent in it.
    pub artifacts: Artifacts,
    /// The runtime identity of the VMs.
    pub identity: RuntimeIdentity,
}

impl Runtime {
    /// Returns the runtime of NVX's artifacts in `directory`, a repository checkout or an
    /// extracted release, which [`Artifacts::from_dir`] finds there, with the guest agent's build
    /// `guest_build`, as the [module documentation](self) describes.
    ///
    /// Fails as [`Artifacts::from_dir`] and [`ArtifactDir::runtime_identity`] do, and with
    /// [`ErrorKind::Unsupported`](nvx::ErrorKind::Unsupported) if the identity of the initramfs
    /// with `guest_build` is not an artifact identity.
    pub fn from_dir(directory: impl AsRef<Path>, guest_build: &str) -> Result<Self> {
        let directory = directory.as_ref();
        let artifacts = Artifacts::from_dir(directory)?;
        let nvx = ArtifactDir::from_dir(directory)?.runtime_identity()?;
        let identity = RuntimeIdentity::new(
            nvx.openvmm,
            nvx.kernel,
            ArtifactIdentity::new(format!(
                "{}+runtime-adapter-guest/{guest_build}",
                nvx.initramfs
            ))?,
        );
        Ok(Self {
            artifacts,
            identity,
        })
    }
}
