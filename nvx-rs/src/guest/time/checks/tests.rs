use super::*;

#[test]
fn the_daemon_is_recognized_by_the_name_that_it_takes() {
    let comm = thread::spawn(|| {
        sys::set_command_name(DAEMON_NAME).expect("set the thread's name");
        std::fs::read("/proc/thread-self/comm").expect("read the thread's name")
    })
    .join()
    .expect("the naming thread panicked");
    assert!(is_daemon_name(&comm));
    assert!(!is_daemon_name(b"nvx-init\n"));
    assert!(!is_daemon_name(b"nvx-time"));
    assert!(!is_daemon_name(b"nvx-time-promot\n"));
}

#[test]
fn kernel_files_are_read_whole_or_fail_their_check_naming_the_file() {
    assert_eq!(
        read_kernel_file(CheckId::C9, kernel::COMMAND_LINE),
        Ok(std::fs::read("/proc/cmdline").expect("read the kernel command line"))
    );
    assert_eq!(
        read_kernel_file(CheckId::C7, c"/proc/nvx-time-missing"),
        Err(Failure::new(
            CheckId::C7,
            "read /proc/nvx-time-missing: No such file or directory"
        ))
    );
}

#[test]
fn wait_timeout_math_uses_the_abi_constants() {
    assert_eq!(STATUS_WAIT, Duration::from_nanos(30_000_000_000));
    assert_eq!(STATUS_POLL, Duration::from_nanos(10_000_000));
    assert_eq!(TIMER_LIST_WAIT, Duration::from_nanos(200_000_000));
}

#[test]
fn identity_constants_match_the_hyperv_leaves() {
    assert_eq!(HYPERV_MAXIMUM_LEAF, 0x4000_0005);
    assert_eq!(HYPERV_VENDOR, [0x7263_694d, 0x666f_736f, 0x7648_2074]);
    assert_eq!(HYPERV_INTERFACE, 0x3123_7648);
    assert_eq!(HYPERV_BUILD, [0x0058_564e, 0x0002_0000]);
    assert_eq!(HYPERV_PRIVILEGES, 0x8860);
    assert_eq!(HYPERV_FEATURES, 0x100);
}

#[test]
fn feature_bits_are_labeled_as_the_c_program_labels_them() {
    let labels = LEAF1_TIME_BITS
        .iter()
        .chain([&TSC_ADJUST_BIT, &RDTSCP_BIT])
        .map(FeatureBit::label)
        .collect::<Vec<_>>();
    assert_eq!(
        labels,
        [
            "CPUID.1:ECX[31] (hypervisor)",
            "CPUID.1:ECX[24] (TSC-deadline)",
            "CPUID.1:ECX[15] (PDCM)",
            "CPUID.1:EDX[4] (TSC)",
            "CPUID.7.0:EBX[1] (TSC_ADJUST)",
            "CPUID.80000001:EDX[27] (RDTSCP)",
        ]
    );
}

#[test]
fn msr_diagnostics_name_the_registers_in_hexadecimal() {
    assert_eq!(format!("{:#010x}", msr::HV_VP_INDEX), "0x40000002");
    assert_eq!(format!("{:#x}", msr::IA32_TSC_DEADLINE), "0x6e0");
    assert_eq!(MsrFile::path(3), "/dev/cpu/3/msr");
}
