//! Opt-in WHP lifecycle proof with a privately supplied native library and guest.
#![cfg(feature = "nvxhost")]

use std::path::PathBuf;
use std::sync::Arc;

use aci_edge_sandboxes::openvmm::{Hypervisor, NvxHostBackend, NvxHostConfig, OpenVmmConfig};
use aci_edge_sandboxes::{
    AciEdgeSandbox, Error, ErrorCode, ExecOutcome, ExecRequest, ProvisionRequest,
};

fn required(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("{name} must be set")))
}

fn approved_digest() -> [u8; 32] {
    let hex = std::env::var("NVXHOST_TEST_SHA256").expect("NVXHOST_TEST_SHA256 must be set");
    assert_eq!(hex.len(), 64, "the approved digest must contain 64 digits");
    let mut digest = [0u8; 32];
    for (index, value) in digest.iter_mut().enumerate() {
        *value = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
            .expect("the approved digest must be hexadecimal");
    }
    digest
}

#[test]
#[ignore = "requires an approved private DLL, edge initramfs, GPT image, and a WHP host"]
fn whp_guest_lifecycle_stops_gracefully_and_cleans_up() {
    let state = tempfile::tempdir().unwrap();
    let config = OpenVmmConfig::new(
        required("NVXHOST_TEST_OPENVMM"),
        required("NVXHOST_TEST_KERNEL"),
        required("NVXHOST_TEST_INITRD"),
        Hypervisor::Whp,
        state.path(),
    );
    let backend = Arc::new(
        NvxHostBackend::new(
            NvxHostConfig::new(
                config,
                required("NVXHOST_TEST_IMAGE"),
                required("NVXHOST_TEST_LIBRARY"),
                approved_digest(),
            )
            .with_guest_debug(true),
        )
        .unwrap(),
    );
    let client = AciEdgeSandbox::from_shared(backend.clone());
    let sandbox_id = client
        .provision(&ProvisionRequest::new())
        .unwrap()
        .sandbox_id;
    let result = (|| {
        let started = client.start(&sandbox_id)?;
        if !started
            .metadata
            .as_ref()
            .is_some_and(|metadata| metadata.contains_key("guestBuildId"))
        {
            return Err(Error::new(
                ErrorCode::BackendError,
                "the guest did not report a build ID",
            ));
        }
        let executed = client
            .exec(&sandbox_id, &ExecRequest::command_line("printf READY"))?
            .wait_with_output()?;
        if executed.outcome != ExecOutcome::Exited(0) || executed.stdout != b"READY" {
            return Err(Error::new(
                ErrorCode::BackendError,
                format!(
                    "guest exec outcome {:?}, stdout {:?}, stderr {:?}",
                    executed.outcome,
                    String::from_utf8_lossy(&executed.stdout),
                    String::from_utf8_lossy(&executed.stderr),
                ),
            ));
        }
        let logs = backend.guest_logs(&sandbox_id)?;
        if !logs.windows(8).any(|window| window == b"execute:") {
            return Err(Error::new(
                ErrorCode::BackendError,
                "guest log stream omitted the executed command",
            ));
        }
        let stopped = client.stop(&sandbox_id)?;
        if stopped
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("forced"))
            .and_then(serde_json::Value::as_bool)
            != Some(false)
        {
            return Err(Error::new(
                ErrorCode::BackendError,
                format!("guest shutdown was not graceful: {:?}", stopped.metadata),
            ));
        }
        let console = std::fs::read(backend.console_log_path(&sandbox_id)).map_err(|error| {
            Error::new(
                ErrorCode::BackendError,
                "cannot read the guest boot console",
            )
            .with_source(error)
        })?;
        if [b"NVX-EDGE-FATAL".as_slice(), b"NVX-EDGE-SHUTDOWN-ERROR"]
            .iter()
            .any(|marker| {
                console
                    .windows(marker.len())
                    .any(|window| window == *marker)
            })
        {
            return Err(Error::new(
                ErrorCode::BackendError,
                "the guest reported a fatal or shutdown error",
            ));
        }
        client.deprovision(&sandbox_id)?;
        if state
            .path()
            .join("nvxhost")
            .join(sandbox_id.token())
            .exists()
        {
            return Err(Error::new(
                ErrorCode::BackendError,
                "deprovision left sandbox state behind",
            ));
        }
        Ok::<_, aci_edge_sandboxes::Error>(())
    })();
    if let Err(error) = result {
        let log = std::fs::read_to_string(backend.log_path(&sandbox_id))
            .unwrap_or_else(|read| format!("cannot read OpenVMM diagnostic log: {read}"));
        let console = std::fs::read_to_string(backend.console_log_path(&sandbox_id))
            .unwrap_or_else(|read| format!("cannot read guest boot console: {read}"));
        let outcome = std::fs::read_to_string(backend.outcome_report_path(&sandbox_id))
            .unwrap_or_else(|read| format!("cannot read OpenVMM outcome report: {read}"));
        let stop = client.stop(&sandbox_id);
        let deprovision = client.deprovision(&sandbox_id);
        panic!(
            "WHP lifecycle failed: {error}; recovery stop: {stop:?}; \
             recovery deprovision: {deprovision:?}; OpenVMM log: {log}; \
             guest console: {console}; OpenVMM outcome: {outcome}"
        );
    }
}
