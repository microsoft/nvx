use super::*;

#[test]
fn msr_values_match_c_alternate_hex() {
    let mut output = Vec::new();
    for value in [0, 1, u64::MAX] {
        print_msr(&mut output, msr::HV_VP_INDEX, "HV_X64_MSR_VP_INDEX", value);
    }
    assert_eq!(
        String::from_utf8(output).unwrap(),
        concat!(
            "msr 0x40000002 HV_X64_MSR_VP_INDEX                = 0 (0)\n",
            "msr 0x40000002 HV_X64_MSR_VP_INDEX                = 0x1 (1)\n",
            "msr 0x40000002 HV_X64_MSR_VP_INDEX                = 0xffffffffffffffff (18446744073709551615)\n",
        )
    );
}

#[test]
fn cpuid_marker_without_hypervisor_matches_c() {
    // Match print_cpuid's defaults when CPUID reports no hypervisor.
    let summary = CpuidSummary {
        vendor: "none".to_owned(),
        ..CpuidSummary::default()
    };
    assert_eq!(
        summary.hypervisor_marker_fields(),
        "hv_vendor=none hv_max_leaf=0 hv_features_eax=0 hv_features_edx=0"
    );
}

#[test]
fn cpuid_marker_keeps_each_feature_register() {
    let summary = CpuidSummary {
        vendor: "Microsoft Hv".to_owned(),
        max_leaf: 0x4000_000b,
        features_eax: 0x2e7f,
        features_edx: 0,
        hypervisor: true,
    };
    assert_eq!(
        summary.hypervisor_marker_fields(),
        "hv_vendor=Microsoft_Hv hv_max_leaf=0x4000000b hv_features_eax=0x2e7f hv_features_edx=0"
    );
}

#[test]
fn hypervisor_fields_match_c_alternate_hex() {
    for (value, expected) in [
        (0, "0"),
        (0x4000_0003, "0x40000003"),
        (u32::MAX, "0xffffffff"),
    ] {
        let summary = CpuidSummary {
            vendor: "Microsoft Hv".to_owned(),
            max_leaf: value,
            features_eax: value,
            features_edx: value,
            hypervisor: true,
        };
        let mut output = Vec::new();
        summary.print_hypervisor_leaf(&mut output);
        assert_eq!(
            String::from_utf8(output).unwrap(),
            format!("cpuid.0x40000000: eax.max_leaf={expected} vendor=\"Microsoft Hv\"\n")
        );
        assert_eq!(
            summary.hypervisor_marker_fields(),
            format!(
                "hv_vendor=Microsoft_Hv hv_max_leaf={expected} hv_features_eax={expected} hv_features_edx={expected}"
            )
        );
    }
}

#[test]
fn token_sanitizing_keeps_only_marker_safe_bytes() {
    assert_eq!(sanitize_token("Microsoft Hv/x"), "Microsoft_Hv_x");
}

#[test]
fn unique_lists_do_not_match_substrings() {
    let mut list = Vec::new();
    add_unique(&mut list, "tsc");
    add_unique(&mut list, "constant_tsc");
    add_unique(&mut list, "tsc");
    assert_eq!(joined(&list, "none"), "tsc,constant_tsc");
    assert_eq!(joined::<&str>(&[], "none"), "none");
}
