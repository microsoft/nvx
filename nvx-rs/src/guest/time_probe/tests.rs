use super::*;
use crate::guest::status::ExitStatus;
use crate::guest::status::tests::command_line;

/// Returns the command line of `nvx-time-probe` with `values`.
fn args(values: &[&[u8]]) -> Vec<CString> {
    command_line("nvx-time-probe", values)
}

#[test]
fn version_help_and_usage_are_byte_exact() {
    let mut out = Vec::new();
    let mut err = Vec::new();
    assert_eq!(run(&args(&[b"version"]), &mut out, &mut err), Ok(()));
    assert_eq!(out, b"nvx-time-probe 1.1\n");
    assert!(err.is_empty());

    out.clear();
    assert_eq!(run(&args(&[b"--help"]), &mut out, &mut err), Ok(()));
    assert_eq!(out, USAGE);

    err.clear();
    assert_eq!(
        run(&args(&[]), &mut out, &mut err),
        Err(CommandError::Usage)
    );
    assert_eq!(err, USAGE);
}

#[test]
fn the_shared_statuses_keep_the_probes_contract() {
    let cases = [
        (Ok(()), 0),
        (Err(CommandError::Operation), 1),
        (Err(CommandError::Usage), 2),
        (Err(CommandError::Setup), 3),
    ];
    for (result, status) in cases {
        assert_eq!(ExitStatus::from(result) as u8, status, "{result:?}");
    }
}

#[test]
#[cfg(target_arch = "aarch64")]
fn arm_refuses_x86_diagnostics_and_counter_frequency_overrides() {
    for arguments in [
        vec![&b"latency"[..]],
        vec![&b"clock"[..]],
        vec![&b"warp"[..], b"--tsc-hz", b"1000000000"],
        vec![&b"warp"[..], b"--tsc-hz=1000000000"],
    ] {
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(
            run(&args(&arguments), &mut out, &mut err),
            Err(CommandError::Usage)
        );
        assert!(out.is_empty());
        assert!(!err.is_empty());
    }
}

#[test]
#[cfg(target_arch = "aarch64")]
fn arm_warp_reports_the_architectural_rate_without_tsc_fields() {
    let cpu = support::affinity_cpus().unwrap()[0].to_string();
    let mut out = Vec::new();
    let mut err = Vec::new();
    assert_eq!(
        run(
            &args(&[b"warp", b"--cpus", cpu.as_bytes()]),
            &mut out,
            &mut err
        ),
        Ok(())
    );
    assert!(err.is_empty());
    let text = String::from_utf8(out).unwrap();
    let frequency = crate::guest::sys::aarch64::counter_frequency_hz().unwrap();
    assert!(text.contains(&format!(
        "counter_hz={frequency} counter_hz_source=cntfrq_el0"
    )));
    assert!(!text.contains("tsc_hz"));
    assert!(text.contains("pairs=0 verdict=PASS bound_ns=1000"));
    assert!(text.contains("conclusive=1"));
}
