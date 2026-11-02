use super::*;

/// Returns the code and message of the error of `result`.
fn refusal(result: Result) -> (ErrorCode, String) {
    match result {
        Err(Exit::Error { code, message }) => (code, message),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_restore_and_mshv_refuse_nested_virtualization() {
    for hypervisor in ["kvm", "whp"] {
        check_request(hypervisor, false).unwrap();
        let (code, message) = refusal(check_request(hypervisor, true));
        assert_eq!(code, ErrorCode::Unsupported);
        assert!(message.starts_with("--nested-virt is not accepted for restore"));
    }
    let (code, message) = refusal(check_request("mshv", false));
    assert_eq!(code, ErrorCode::Unsupported);
    assert!(message.starts_with("--nested-virt requires the KVM or WHP hypervisor"));
}

#[test]
fn only_an_intel_cpu_takes_nested_virtualization() {
    let offered = || Ok("Y\n".to_owned());
    for hypervisor in ["kvm", "whp"] {
        check_host_with(hypervisor, Some(INTEL), offered).unwrap();
        check_host_with(hypervisor, None, offered).unwrap();
        let (code, message) = refusal(check_host_with(hypervisor, Some("AuthenticAMD"), offered));
        assert_eq!(code, ErrorCode::Unsupported);
        assert_eq!(
            message,
            "--nested-virt requires an Intel CPU, not AuthenticAMD; a microVM exposes VMX, \
             and no CPU profile describes AMD's SVM"
        );
    }
}

#[test]
fn kvm_must_offer_nested_vmx() {
    for value in ["Y\n", "1\n", "Y"] {
        check_host_with("kvm", Some(INTEL), || Ok(value.to_owned())).unwrap();
    }
    for (value, reason) in [
        ("N\n", format!("{KVM_INTEL_NESTED} is N")),
        ("0\n", format!("{KVM_INTEL_NESTED} is 0")),
    ] {
        let (code, message) = refusal(check_host_with("kvm", Some(INTEL), || Ok(value.to_owned())));
        assert_eq!(code, ErrorCode::Unavailable);
        assert_eq!(
            message,
            format!(
                "--nested-virt requires KVM's nested virtualization, which this host does \
                 not offer: {reason}; load kvm_intel with nested=1"
            )
        );
    }
    let (code, message) = refusal(check_host_with("kvm", Some(INTEL), || {
        Err(io::ErrorKind::NotFound.into())
    }));
    assert_eq!(code, ErrorCode::Unavailable);
    assert!(message.contains("kvm_intel is not loaded"), "{message}");
    let (code, message) = refusal(check_host_with("kvm", Some(INTEL), || {
        Err(io::ErrorKind::PermissionDenied.into())
    }));
    assert_eq!(code, ErrorCode::Unavailable);
    assert!(
        message.contains(&format!("cannot read {KVM_INTEL_NESTED}")),
        "{message}"
    );
    // WHP's own check is OpenVMM's.
    check_host_with("whp", Some(INTEL), || Err(io::ErrorKind::NotFound.into())).unwrap();
}
