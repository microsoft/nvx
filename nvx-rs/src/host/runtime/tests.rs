//! Tests of the runtime identities, of the values that every launch shares, and of the probe of a
//! hypervisor.

use super::*;
use crate::ErrorKind;
/// Returns the identity `sha256:` and 64 copies of `digit`.
fn artifact(digit: char) -> ArtifactIdentity {
    ArtifactIdentity::sha256(&digit.to_string().repeat(64)).unwrap()
}

/// Returns a runtime identity under this crate's contracts.
fn runtime() -> RuntimeIdentity {
    RuntimeIdentity::new(artifact('a'), artifact('b'), artifact('c'))
}

#[test]
fn artifact_identities_are_printable_tokens() {
    assert_eq!(
        ArtifactIdentity::new("openvmm-1.2+build.7")
            .unwrap()
            .as_str(),
        "openvmm-1.2+build.7"
    );
    for invalid in [
        String::new(),
        "with space".to_owned(),
        "tab\there".to_owned(),
        "caf\u{e9}".to_owned(),
        "x".repeat(MAX_IDENTITY_LEN + 1),
    ] {
        let error = ArtifactIdentity::new(invalid.clone()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{invalid:?}");
    }
    assert!(ArtifactIdentity::new("x".repeat(MAX_IDENTITY_LEN)).is_ok());

    let digest = "AB".repeat(32);
    assert_eq!(
        ArtifactIdentity::sha256(&digest).unwrap().to_string(),
        format!("sha256:{}", "ab".repeat(32))
    );
    for invalid in ["ab".repeat(31), "g".repeat(64), "ab".repeat(33)] {
        assert_eq!(
            ArtifactIdentity::sha256(&invalid).unwrap_err().kind(),
            ErrorKind::Unsupported
        );
    }
    // The serde form checks the token too.
    assert!(serde_json::from_value::<ArtifactIdentity>(serde_json::json!("a b")).is_err());
}

#[test]
fn runtime_identities_bind_this_crates_contracts() {
    let identity = runtime();
    assert_eq!(identity.boot_contract, contract::BOOT_CONTRACT_VERSION);
    assert_eq!(
        identity.control_contract,
        contract::CONTROL_CONTRACT_REVISION
    );
    assert_eq!(
        identity.control_session,
        contract::CONTROL_SESSION_PROTOCOL_VERSION
    );
    identity.check().unwrap();
    assert!(identity.differences(&runtime()).is_empty());

    let mut other = runtime();
    other.kernel = artifact('d');
    other.control_session += 1;
    assert_eq!(
        identity.differences(&other),
        [IdentityPart::Kernel, IdentityPart::ControlSession]
    );
    other = RuntimeIdentity::new(artifact('e'), artifact('b'), artifact('f'));
    other.boot_contract += 1;
    other.control_contract = "another-revision".to_owned();
    assert_eq!(
        identity.differences(&other),
        [
            IdentityPart::OpenVmm,
            IdentityPart::Initramfs,
            IdentityPart::BootContract,
            IdentityPart::ControlContract
        ]
    );
    let names: Vec<_> = IdentityPart::ALL.iter().map(ToString::to_string).collect();
    assert_eq!(
        names,
        [
            "OpenVMM",
            "kernel",
            "initramfs",
            "boot contract",
            "control contract",
            "control session"
        ]
    );
    for invalid in ["", "two words"] {
        other.control_contract = invalid.to_owned();
        assert_eq!(other.check().unwrap_err().kind(), ErrorKind::Unsupported);
    }
}

/// Returns a runtime that supplies each field that every launch shares, with the default launch
/// options.
fn shared() -> Runtime {
    Runtime {
        openvmm: PathBuf::from("openvmm"),
        kernel: PathBuf::from("vmlinux"),
        initramfs: PathBuf::from("initramfs.cpio.gz"),
        identity: runtime(),
        hypervisor: Hypervisor::Kvm,
        cpu_profile: Some("auto".to_owned()),
        windows_breakaway: false,
        options: LaunchOptions::default(),
    }
}

#[test]
fn by_default_a_guest_that_powers_off_ends_its_vm() {
    assert_eq!(GuestPowerAction::default(), GuestPowerAction::Exit);
    assert_eq!(
        shared().options,
        LaunchOptions {
            guest_shutdown: GuestPowerAction::Exit,
            control_auth_timeout: None,
        }
    );
}

#[test]
fn a_hypervisor_that_the_host_never_has_is_unavailable() {
    let foreign: &[Hypervisor] = if cfg!(target_os = "linux") {
        &[Hypervisor::Whp]
    } else if cfg!(windows) {
        &[Hypervisor::Kvm, Hypervisor::Mshv]
    } else {
        &[Hypervisor::Kvm, Hypervisor::Mshv, Hypervisor::Whp]
    };
    for &hypervisor in foreign {
        let error = probe_hypervisor(hypervisor).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unavailable, "{hypervisor}");
        assert_eq!(
            error.to_string(),
            platform::probe_hypervisor(hypervisor).unwrap_err(),
            "{hypervisor}"
        );
    }
}
