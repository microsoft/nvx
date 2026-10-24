//! Versions of the contracts that this crate speaks with OpenVMM and the NVX guest, and the
//! compatibility of NVX releases with them.
//!
//! The store records the boot contract, control contract, and control session versions of the
//! build that started a VM in the VM's runtime identity, `nvx::host::store::RuntimeIdentity`, and a
//! build drives only the VMs whose runtime identity is its own. So each version changes whenever a
//! guest that a build of the earlier version started could not be driven as one that a build of
//! the later version starts:
//!
//! - [`MICROVM_ABI_VERSION`] is the microVM ABI of the pinned OpenVMM: the machine, its devices,
//!   and their addresses, through which OpenVMM boots the guest and saves its state. It changes
//!   only with an OpenVMM that changes that ABI, and the boot contract changes with it.
//! - [`BOOT_CONTRACT_VERSION`] is the contract between the host and the guests that it boots: the
//!   microVM ABI, the kernel command-line tokens through which the host configures the guest, such
//!   as the boot tokens of a layered root in [`protocol::root`](crate::protocol::root), and the
//!   root that the guest assembles from them. It changes with any of them, such as a token that
//!   is added, removed, or read another way, an image format or upper of a layered root, or a
//!   change to how the guest assembles or mounts that root.
//! - [`CONTROL_SESSION_PROTOCOL_VERSION`] is the version of the control session, through which the
//!   host reaches the guest agent over OpenVMM's control console. It changes with the session's
//!   framing, records, credits, or authentication.
//! - [`CONTROL_CONTRACT_REVISION`] is the revision of the control contract that the session
//!   carries: the requests, responses, and features of the guest agent. It changes with them.
//!
//! A release declares the microVM ABI version, the control session version, and the control
//! contract revision of its artifacts in its `SOURCE-MANIFEST.json`.
//!
//! The compatibility key of a template, `nvx::host::template::TemplateKey`, holds the boot and
//! control versions, and the versions of the contracts through which a template's clones are
//! restored, which no runtime identity records:
//!
//! - [`SNAPSHOT_FORMAT_VERSION`] is the version of OpenVMM's snapshot format: the manifest of a
//!   capture, and the artifacts that it describes. It changes with the version of OpenVMM's
//!   snapshot manifest, which refuses a snapshot of any other.
//! - [`DEVICE_STATE_SCHEMA_VERSION`] is the version of the schema of the saved state of a
//!   snapshot's devices, which OpenVMM records in its manifest. It changes with it.
//! - [`TIME_ABI_VERSION`] is the version of the time ABI: the guest's clocks and timers, and their
//!   repair after a restore. It changes with the time ABI version of OpenVMM's time contract.
//! - [`RESTORE_PACKET_VERSION`] is the version of the restore packet that OpenVMM gives a
//!   restored guest, and that the guest decodes. It changes with the packet's layout or meaning.
//!
//! Each of these four must match the pinned OpenVMM's, which the ignored test
//! `tests/openvmm_vectors.rs` checks in the jobs that check out the OpenVMM submodule. The key
//! also holds the version of the activation contract,
//! [`activation::VERSION`](crate::activation::VERSION), through which each clone receives the
//! settings of its own launch.
//!
//! The build script includes this file to check bundled artifacts, so it must not depend on the
//! rest of the crate.

/// OpenVMM microVM ABI version this crate drives.
pub const MICROVM_ABI_VERSION: u64 = 2;
/// Version of the boot contract this crate speaks; see the [module](self) for when it changes.
///
/// Until version 3, the runtime identity recorded the microVM ABI version, 2, as the boot
/// contract. Version 3 names the format of a layered root's image, `erofs` or `gpt`, in its boot
/// token, and keeps the root's upper in RAM if asked.
pub const BOOT_CONTRACT_VERSION: u64 = 3;
/// Control-console broker protocol version this crate speaks.
pub const CONTROL_SESSION_PROTOCOL_VERSION: u64 = 1;
/// Control contract revision this crate speaks.
pub const CONTROL_CONTRACT_REVISION: &str = "nvx-microvm-v2-control-v2";
/// Version of OpenVMM's snapshot format that this crate reads: the manifest of a capture, and the
/// artifacts that it describes. It must match `MANIFEST_VERSION` in OpenVMM's
/// `openvmm/openvmm_helpers/src/snapshot.rs`.
pub const SNAPSHOT_FORMAT_VERSION: u64 = 6;
/// Version of the schema of a snapshot's saved device state that this crate reads. It must match
/// `SAVED_STATE_SCHEMA_VERSION` in OpenVMM's `openvmm/openvmm_helpers/src/snapshot/format.rs`.
pub const DEVICE_STATE_SCHEMA_VERSION: u64 = 1;
/// Version of the time ABI that this crate speaks: the guest's clocks and timers, and their repair
/// after a restore. It must match `TIME_ABI_VERSION` in OpenVMM's
/// `vmm_core/virt/src/time_abi/mod.rs`.
pub const TIME_ABI_VERSION: u64 = 1;
/// Version of the restore packet that OpenVMM gives a restored guest, and that this crate's guest
/// decodes. It must match `RESTORE_PACKET_VERSION` in OpenVMM's
/// `vm/devices/chipset_resources/src/microvm_time.rs`.
pub const RESTORE_PACKET_VERSION: u64 = 4;

/// Returns whether the `openvmm` section of an NVX `SOURCE-MANIFEST.json` declares this contract.
#[cfg(feature = "host")]
pub fn manifest_is_compatible(manifest: &serde_json::Value) -> bool {
    let field = |name: &str| {
        manifest
            .get("openvmm")
            .and_then(|section| section.get(name))
    };
    field("microvm_abi_version").and_then(serde_json::Value::as_u64) == Some(MICROVM_ABI_VERSION)
        && field("control_session_protocol_version").and_then(serde_json::Value::as_u64)
            == Some(CONTROL_SESSION_PROTOCOL_VERSION)
        && field("control_contract_revision").and_then(serde_json::Value::as_str)
            == Some(CONTROL_CONTRACT_REVISION)
}
