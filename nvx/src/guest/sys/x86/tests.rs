//! Checks of the instructions on the build host.

use super::*;

#[test]
fn the_time_stamp_counter_advances_through_every_read() {
    let first = rdtsc_fenced();
    let second = rdtsc_lfence();
    let third = rdtsc();
    assert!(
        first <= second && second <= third,
        "{first} {second} {third}"
    );
    if has_rdtscp() {
        assert!(rdtscp() >= third);
    }
}

#[test]
fn cpuid_reports_the_time_stamp_counter() {
    let features = cpuid(cpuid::LEAF_FEATURES, 0);
    assert!(cpuid::bit(features.edx, cpuid::FEATURES_EDX_TSC));
    assert!(!cpuid::bit(0, 0) && cpuid::bit(1 << 31, 31));
    assert_eq!(cpuid::Register::Edx.of(features), features.edx);
    assert_eq!(cpuid::Register::Ebx.name(), "EBX");
}

#[test]
fn signatures_decode_the_display_family_and_model() {
    // An Intel Xeon Platinum 8573C, family 6 model 0xCF stepping 2.
    let intel = cpuid::Signature::decode(0x000c_06f2);
    assert_eq!((intel.family, intel.model, intel.stepping), (6, 0xcf, 2));
    // An AMD EPYC 7763, family 0x19 model 1 stepping 1.
    let amd = cpuid::Signature::decode(0x00a0_0f11);
    assert_eq!((amd.family, amd.model, amd.stepping), (0x19, 1, 1));
    // Other families keep the base model.
    let other = cpuid::Signature::decode(0x0001_0521);
    assert_eq!((other.family, other.model, other.stepping), (5, 2, 1));
}
