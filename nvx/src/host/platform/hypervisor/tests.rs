use super::*;
use crate::ErrorKind;

#[test]
fn hypervisors_parse_their_openvmm_spelling() {
    for hypervisor in [Hypervisor::Kvm, Hypervisor::Mshv, Hypervisor::Whp] {
        assert_eq!(
            hypervisor.as_str().parse::<Hypervisor>().unwrap(),
            hypervisor
        );
        assert_eq!(
            serde_json::to_string(&hypervisor).unwrap(),
            format!("\"{hypervisor}\"")
        );
        assert_eq!(
            serde_json::from_str::<Hypervisor>(&format!("\"{hypervisor}\"")).unwrap(),
            hypervisor
        );
    }
    let error = "hyperv".parse::<Hypervisor>().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable);
    assert_eq!(
        error.message(),
        "unknown hypervisor \"hyperv\"; choose kvm, mshv, or whp"
    );
}

#[test]
fn the_platform_default_runs_on_the_host() {
    if let Some(hypervisor) = Hypervisor::platform_default() {
        assert!(hypervisor.supported_on_host());
    }
    assert!(!(Hypervisor::Kvm.supported_on_host() && Hypervisor::Whp.supported_on_host()));
}
