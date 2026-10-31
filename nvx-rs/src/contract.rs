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
//! A release declares the microVM ABI, time ABI, restore-packet, control-session, and control
//! contract versions of its artifacts in its `SOURCE-MANIFEST.json`.
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
//! `tests/openvmm_vectors.rs` checks in the jobs that check out the OpenVMM submodule. So must
//! [`RESTORE_PACKET_NETWORK_VERSION`], the version of the packet that OpenVMM gives instead at a
//! restore that replaces the network policy of a snapshot without a tier, which the guest decodes
//! too. No clone of a template is restored so, so the key does not hold it. The key
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
///
/// Version 2 pairs each time sample's host UTC with the guest TSC on the host, and publishes the
/// host's clock correction with it, which the guest applies instead of estimating it.
pub const TIME_ABI_VERSION: u64 = 2;
/// Version of the restore packet that OpenVMM gives a restored guest, and that this crate's guest
/// decodes. It must match `RESTORE_PACKET_VERSION` in OpenVMM's
/// `vm/devices/chipset_resources/src/microvm_time.rs`.
///
/// Version 5 carries no host UTC: the guest takes a time sample to set its clock.
pub const RESTORE_PACKET_VERSION: u64 = 5;
/// Version of the restore packet that OpenVMM gives a restored guest in place of
/// [`RESTORE_PACKET_VERSION`]'s at a restore that replaces the network policy of a snapshot
/// without a tier: that version's packet, with the new policy's DNS server and whether the guest
/// announces its listeners under it, which a guest that decodes only the other version refuses.
/// This crate's guest decodes both. It must match `RESTORE_PACKET_NETWORK_VERSION` in OpenVMM's
/// `vm/devices/chipset_resources/src/microvm_time.rs`.
pub const RESTORE_PACKET_NETWORK_VERSION: u64 = 6;

/// Returns how the `openvmm` section of an NVX `SOURCE-MANIFEST.json` departs from the contracts
/// that this crate speaks: one `NAME: expected VALUE, found VALUE` entry, in JSON, for each field
/// that it declares otherwise or not at all, so that an error can name every mismatch.
#[cfg(feature = "host")]
pub fn manifest_mismatches(manifest: &serde_json::Value) -> Vec<String> {
    let section = manifest.get("openvmm");
    [
        (
            "microvm_abi_version",
            serde_json::Value::from(MICROVM_ABI_VERSION),
        ),
        ("time_abi_version", TIME_ABI_VERSION.into()),
        ("restore_packet_version", RESTORE_PACKET_VERSION.into()),
        (
            "control_session_protocol_version",
            CONTROL_SESSION_PROTOCOL_VERSION.into(),
        ),
        (
            "control_contract_revision",
            CONTROL_CONTRACT_REVISION.into(),
        ),
    ]
    .into_iter()
    .filter_map(|(name, expected)| {
        let declared = section.and_then(|section| section.get(name));
        (declared != Some(&expected)).then(|| {
            let found = declared.map_or_else(|| "none".to_owned(), ToString::to_string);
            format!("{name}: expected {expected}, found {found}")
        })
    })
    .collect()
}

/// Returns whether the `openvmm` section of an NVX `SOURCE-MANIFEST.json` declares this contract.
#[cfg(feature = "host")]
pub fn manifest_is_compatible(manifest: &serde_json::Value) -> bool {
    manifest_mismatches(manifest).is_empty()
}

#[cfg(all(test, feature = "host"))]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn release_manifests_must_match_every_bundled_contract() {
        let manifest = json!({
            "openvmm": {
                "microvm_abi_version": MICROVM_ABI_VERSION,
                "time_abi_version": TIME_ABI_VERSION,
                "restore_packet_version": RESTORE_PACKET_VERSION,
                "control_session_protocol_version": CONTROL_SESSION_PROTOCOL_VERSION,
                "control_contract_revision": CONTROL_CONTRACT_REVISION,
            }
        });
        assert!(manifest_is_compatible(&manifest));
        for field in [
            "microvm_abi_version",
            "time_abi_version",
            "restore_packet_version",
            "control_session_protocol_version",
            "control_contract_revision",
        ] {
            let mut incompatible = manifest.clone();
            incompatible["openvmm"][field] = serde_json::Value::Null;
            assert!(!manifest_is_compatible(&incompatible), "{field}");
        }
    }

    #[test]
    fn mismatches_name_each_field_with_its_expected_and_declared_values() {
        // A release from before time ABI 2 declares the control revision but neither the time
        // ABI nor the restore packet, so naming the revision alone would not tell what differs.
        let manifest = json!({
            "openvmm": {
                "microvm_abi_version": MICROVM_ABI_VERSION,
                "control_session_protocol_version": 2,
                "control_contract_revision": CONTROL_CONTRACT_REVISION,
            }
        });
        assert_eq!(
            manifest_mismatches(&manifest),
            [
                format!("time_abi_version: expected {TIME_ABI_VERSION}, found none"),
                format!("restore_packet_version: expected {RESTORE_PACKET_VERSION}, found none"),
                format!(
                    "control_session_protocol_version: expected \
                     {CONTROL_SESSION_PROTOCOL_VERSION}, found 2"
                ),
            ]
        );
        let other = json!({"openvmm": {"control_contract_revision": "other"}});
        assert!(manifest_mismatches(&other).contains(&format!(
            "control_contract_revision: expected \"{CONTROL_CONTRACT_REVISION}\", found \"other\""
        )));
        assert_eq!(manifest_mismatches(&json!({})).len(), 5);
    }
}
