use super::*;
use crate::host::process::BootId;
use crate::host::runtime::{ArtifactIdentity, CompatibilityIdentity};

fn runtime(compatibility: &str) -> RuntimeIdentity {
    let artifact = ArtifactIdentity::new("recorded-artifact").unwrap();
    RuntimeIdentity::new(
        artifact.clone(),
        artifact.clone(),
        artifact,
        CompatibilityIdentity::new(compatibility).unwrap(),
    )
}

fn record() -> RuntimeRecord {
    RuntimeRecord {
        version: 2,
        architecture: GuestArchitecture::Aarch64,
        vm_dir: "private-vm".into(),
        endpoint: "\0must-not-open".into(),
        capability: [5; CAPABILITY_LEN],
        process: ProcessRef {
            pid: 0,
            start_time: 0,
            boot: BootId::from_bytes([0; 16]),
        },
        runtime: runtime("nvx-boot-v5+nvxs-v1+nvx-microvm-v2-control-v2"),
    }
}

#[test]
fn current_records_require_explicit_supported_version_and_architecture() {
    let value = serde_json::to_value(record()).unwrap();
    let read = serde_json::from_value::<RuntimeRecord>(value.clone()).unwrap();
    assert_eq!(read.architecture, GuestArchitecture::Aarch64);
    for (field, replacement) in [
        ("version", Some(serde_json::json!(3))),
        ("version", Some(serde_json::Value::Null)),
        ("architecture", None),
        ("architecture", Some(serde_json::json!("riscv64"))),
        ("architecture", Some(serde_json::Value::Null)),
        ("version", None),
    ] {
        let mut malformed = value.clone();
        if let Some(replacement) = replacement {
            malformed[field] = replacement;
        } else {
            malformed.as_object_mut().unwrap().remove(field);
        }
        assert!(serde_json::from_value::<RuntimeRecord>(malformed).is_err());
    }
}

#[test]
fn only_the_recognized_unversioned_x86_producer_record_gets_legacy_treatment() {
    let mut legacy = serde_json::to_value(record()).unwrap();
    legacy.as_object_mut().unwrap().remove("version");
    legacy.as_object_mut().unwrap().remove("architecture");
    legacy["runtime"]["compatibility"] =
        serde_json::json!("nvx-boot-v5+nvxs-v1+nvx-microvm-v2-control-v2");
    let read = serde_json::from_value::<RuntimeRecord>(legacy.clone()).unwrap();
    assert_eq!(read.architecture, GuestArchitecture::X86_64);
    assert_eq!(read.version, 2);
    legacy["runtime"]["compatibility"] = serde_json::json!("unrecognized-caller-contract");
    assert!(serde_json::from_value::<RuntimeRecord>(legacy).is_err());
}
