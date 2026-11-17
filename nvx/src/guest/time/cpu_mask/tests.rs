use super::*;

#[test]
fn lists_overlap_and_stop_below_the_abi_limit() {
    let mask = CpuMask::parse(b"0-3,2,5").unwrap();
    assert_eq!(mask.len(), 5);
    assert_eq!(mask.iter().collect::<Vec<_>>(), [0, 1, 2, 3, 5]);
    assert!(mask.contains(5) && !mask.contains(4) && !mask.contains(64));
    assert_eq!(CpuMask::parse(b"63"), Some(CpuMask::EMPTY.with(63)));
    assert_eq!(CpuMask::parse(b"64"), None);
    assert_eq!(CpuMask::parse(b"3-1"), None);
    assert_eq!(CpuMask::parse(b"0-3\n"), None);
    assert_eq!(CpuMask::parse(b""), Some(CpuMask::EMPTY));
}

#[test]
fn new_cpus_may_be_none() {
    assert_eq!(CpuMask::parse_new(b"none"), Some(CpuMask::EMPTY));
    assert_eq!(CpuMask::parse_new(b""), Some(CpuMask::EMPTY));
    assert_eq!(CpuMask::parse_new(b"1-3,5").map(CpuMask::len), Some(4));
    assert_eq!(CpuMask::parse_new(b"64"), None);
}

#[test]
fn masks_print_as_kernel_cpu_lists() {
    let mask = CpuMask::parse(b"4,5,6,9,11-12,63").unwrap();
    assert_eq!(mask.to_string(), "4-6,9,11-12,63");
    assert_eq!(CpuMask::EMPTY.to_string(), "none");
    assert_eq!(CpuMask::parse(&mask.to_string().into_bytes()), Some(mask));
}
