use super::*;

#[test]
fn common_kernel_violations_and_own_records_have_one_watcher_policy() {
    for (message, code) in [
        (
            "clocksource: Switched to clocksource jiffies",
            ViolationCode::ClocksourceSwitch,
        ),
        (
            "watchdog: BUG: soft lockup - CPU#0 stuck",
            ViolationCode::SoftLockup,
        ),
        (
            "INFO: task worker blocked for more than 30 seconds",
            ViolationCode::HungTask,
        ),
    ] {
        assert_eq!(match_watch_rule(message.as_bytes()), Some(code));
        assert_eq!(
            match_watch_rule(format!("NVX-TIME-EVENT: {message}").as_bytes()),
            None
        );
    }
}

#[test]
#[cfg(target_arch = "aarch64")]
fn arm_runtime_has_no_x86_tsc_msr_or_apic_watcher_admission() {
    for message in [
        "Marking TSC unstable due to observed drift",
        "TSC synchronization [CPU#1 -> CPU#0]",
        "TSC ADJUST changed",
        "unchecked MSR access error",
        "APIC ID mismatch",
    ] {
        assert_eq!(match_watch_rule(message.as_bytes()), None);
    }
}
