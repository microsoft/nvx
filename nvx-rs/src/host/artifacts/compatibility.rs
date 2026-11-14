//! The compatibility of the bundled guests that NVX's CLI drives: the versions of its contracts
//! with them, which it names in the runtime identity of each VM that it starts.
//!
//! The library declares none of these. A VM's
//! [`RuntimeIdentity`](crate::host::runtime::RuntimeIdentity) holds whatever
//! [`CompatibilityIdentity`] its caller names, and the CLI names [`compatibility`], which joins
//! the versions below. The CLI drives only the VMs whose runtime identity is the one that its
//! artifacts give, so each version changes whenever a guest that a build of the earlier version
//! started could not be driven as one that a build of the later version starts:
//!
//! - [`BOOT_CONTRACT_VERSION`] is the contract between the CLI and the guests that it boots: the
//!   microVM ABI, the kernel command-line tokens through which the CLI configures the guest, such
//!   as the boot tokens of a layered root in [`protocol::root`](crate::protocol::root), and the
//!   root that the bundled init assembles from them. It changes with any of them, such as a token
//!   that is added, removed, or read another way, an image format or upper of a layered root, or
//!   a change to how the guest assembles or mounts that root.
//! - [`CONTROL_SESSION_PROTOCOL_VERSION`] is the version of the control session, through which the
//!   CLI reaches the guest agent over OpenVMM's control console. It changes with the session's
//!   framing, records, credits, or authentication.
//! - [`CONTROL_CONTRACT_REVISION`] is the revision of the control contract that the session
//!   carries: the requests and responses of NVX's own control protocol, which NVX's guest agents
//!   serve. It changes with them.
//!
//! A release declares the microVM ABI, time ABI, restore-packet, control-session, and control
//! contract versions of its artifacts in its `SOURCE-MANIFEST.json`, which `nvx-rs download`
//! checks against these and against the versions that the CLI's snapshot decoder reads.

use crate::error::Result;
use crate::host::runtime::CompatibilityIdentity;

/// Version of the boot contract between the CLI and its bundled guests; see the [module](self)
/// for when it changes.
///
/// Until version 3, the runtime identity recorded the microVM ABI version, 2, as the boot
/// contract. Version 3 names the format of a layered root's image, `erofs` or `gpt`, in its boot
/// token, and keeps the root's upper in RAM if asked. Version 4 boots guests on microVM ABI 3.
/// Version 5 boots them on OpenVMM's v4 microVM, whose restore packet and time samples the guest
/// decodes at version 1.
pub(crate) const BOOT_CONTRACT_VERSION: u64 = 5;
/// Version of the control session through which the CLI reaches the bundled guest agent.
pub(crate) const CONTROL_SESSION_PROTOCOL_VERSION: u64 = 1;
/// Revision of the control contract that the bundled guest agent serves.
pub(crate) const CONTROL_CONTRACT_REVISION: &str = "nvx-microvm-v2-control-v2";
/// Version of the restore packet that the bundled guest decodes, which a release must declare. It
/// must match the guest's decoder and `RESTORE_PACKET_VERSION` in OpenVMM's
/// `vm/devices/chipset_resources/src/microvm_time.rs`, which the ignored test
/// `tests/openvmm_vectors.rs` checks.
pub(crate) const RESTORE_PACKET_VERSION: u64 = 1;

/// Returns the compatibility identity that the CLI names in the runtime identity of each VM that
/// it starts: `nvx-boot-vB+nvxs-vS+R`, of the [boot contract](BOOT_CONTRACT_VERSION), the
/// [control session](CONTROL_SESSION_PROTOCOL_VERSION), and the
/// [control contract](CONTROL_CONTRACT_REVISION).
pub(crate) fn compatibility() -> Result<CompatibilityIdentity> {
    CompatibilityIdentity::new(format!(
        "nvx-boot-v{BOOT_CONTRACT_VERSION}+nvxs-v{CONTROL_SESSION_PROTOCOL_VERSION}+\
         {CONTROL_CONTRACT_REVISION}"
    ))
}

#[cfg(test)]
mod tests;
