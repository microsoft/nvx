//! Versions of the OpenVMM control contract this crate speaks.
//!
//! The build script includes this file to check bundled artifacts, so it must not depend on the
//! rest of the crate.

/// OpenVMM microVM ABI version this crate drives.
pub(crate) const MICROVM_ABI_VERSION: u64 = 2;
/// Control-console broker protocol version this crate speaks.
pub(crate) const CONTROL_SESSION_PROTOCOL_VERSION: u64 = 1;
/// Control contract revision this crate speaks.
pub(crate) const CONTROL_CONTRACT_REVISION: &str = "nvx-microvm-v2-control-v1";

/// Returns whether the `openvmm` section of an NVX `SOURCE-MANIFEST.json` declares this contract.
pub(crate) fn manifest_is_compatible(manifest: &serde_json::Value) -> bool {
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
