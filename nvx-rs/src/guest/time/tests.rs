use super::*;
use crate::guest::status::tests::command_line;

#[test]
fn both_architectures_require_enabled_watchdogs_when_the_interfaces_exist() {
    assert!(system::debug_watchdog_policy(None, None).is_ok());
    assert!(system::debug_watchdog_policy(Some(1), Some(120)).is_ok());
    for (soft, hung) in [
        (Some(0), Some(120)),
        (Some(1), Some(0)),
        (None, Some(120)),
        (Some(1), None),
    ] {
        assert!(system::debug_watchdog_policy(soft, hung).is_err());
    }
}

/// Returns the command line of `nvx-time` with `values`.
fn args(values: &[&[u8]]) -> Vec<CString> {
    command_line("nvx-time", values)
}

#[test]
fn usage_is_byte_exact_and_bounds_the_arguments() {
    let mut out = Vec::new();
    let mut err = Vec::new();
    assert_eq!(
        run(&args(&[]), &mut out, &mut err),
        Err(CommandError::Usage)
    );
    assert_eq!(err, USAGE);
    err.clear();
    let many = vec![b"x".as_slice(); MAX_ARGUMENTS + 1];
    assert_eq!(
        run(&args(&many), &mut out, &mut err),
        Err(CommandError::Usage)
    );
    assert_eq!(err, USAGE);
    assert!(out.is_empty());
}

#[test]
fn checked_phases_have_state_file_positions() {
    let positions = Phase::CHECKS.map(Phase::check_index);
    assert_eq!(positions, [Some(0), Some(1), Some(2)]);
    assert_eq!(Phase::Runtime.check_index(), None);
}

#[test]
#[cfg(target_arch = "aarch64")]
fn coldboot_cli_rejects_snapshot_commands_without_accessing_the_platform() {
    for command in [
        "capture",
        "restore-finish",
        "pre-capture",
        "cancel-capture",
        "check",
    ] {
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(
            run(&args(&[command.as_bytes()]), &mut out, &mut err),
            Err(CommandError::Usage)
        );
        assert_eq!(err, USAGE);
        assert!(out.is_empty());
    }
}
