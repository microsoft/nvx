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
