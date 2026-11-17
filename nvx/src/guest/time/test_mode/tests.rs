//! Focused regressions for the conversions of the test hooks.

use super::*;

fn hook(arguments: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    run(arguments, &mut out).unwrap();
    out
}

#[test]
fn clear_phase_changes_only_the_pll_offset() {
    let mut adjustment = None;
    assert_eq!(
        test_clear_phase_with(&[], |value| {
            adjustment = Some(value);
            Ok(())
        }),
        Ok(())
    );
    assert_eq!(adjustment, Some(ClockAdjustment::ClearPhase));
    assert_eq!(
        test_clear_phase_with(&[b"extra"], |_| panic!("unexpected adjustment")),
        Err(CommandError::Usage)
    );
    assert_eq!(
        test_clear_phase_with(&[], |_| Err(Errno::EPERM)),
        Err(CommandError::Operation)
    );
}

#[test]
fn cpuid_values_are_32_bit_decimal_or_hexadecimal() {
    assert_eq!(cpuid_value(b"0x40000006"), Ok(0x4000_0006));
    assert_eq!(cpuid_value(b"7"), Ok(7));
    assert_eq!(cpuid_value(b"4294967295"), Ok(u32::MAX));
    for text in [&b"4294967296"[..], b"-1", b"x", b""] {
        assert_eq!(cpuid_value(text), Err(CommandError::Usage), "{text:?}");
    }
}

#[test]
fn malformed_hooks_fail_with_the_usage_status() {
    /// The arguments of a well-formed `pair` hook.
    const PAIR: [&[u8]; 11] = [
        b"pair",
        b"1000000000000000000",
        b"1000000",
        b"500",
        b"999000",
        b"1002000",
        b"999999999999000000",
        b"1004000",
        b"40",
        b"2000000000",
        b"0",
    ];
    let mut out = Vec::new();
    assert_eq!(run(&[], &mut out), Err(CommandError::Usage));
    assert_eq!(run(&[b"unknown"], &mut out), Err(CommandError::Usage));
    assert_eq!(run(&[b"pair", b"1"], &mut out), Err(CommandError::Usage));
    // Time ABI 1's pairing of a realtime bracket is no longer a hook.
    assert_eq!(
        run(&[b"pair", b"1000", b"3000", b"5000"], &mut out),
        Err(CommandError::Usage)
    );
    let mut plus = PAIR;
    plus[2] = b"+1000000";
    assert_eq!(run(&plus, &mut out), Err(CommandError::Usage));
    let mut negative = PAIR;
    negative[3] = b"-500";
    assert_eq!(run(&negative, &mut out), Err(CommandError::Usage));
    assert_eq!(
        run(&[b"frequency", b"-40000000"], &mut out),
        Err(CommandError::Usage)
    );
    assert_eq!(
        run(
            &[b"frequency", b"9223372036854775808", b"2100000000"],
            &mut out
        ),
        Err(CommandError::Usage)
    );
    assert_eq!(
        run(&[b"frequency", b"0", b"-1"], &mut out),
        Err(CommandError::Usage)
    );
    assert_eq!(
        run(&[b"correction", b"-5", b"10", b"slow"], &mut out),
        Err(CommandError::Usage)
    );
    assert_eq!(
        run(&[b"deferral", b"x"], &mut out),
        Err(CommandError::Usage)
    );
    // Only malformed: a well-formed probe without the permission ends the process.
    for arguments in [
        &[b"port-access".as_slice(), b"x"][..],
        &[b"port-probe"],
        &[b"port-probe", b"control"],
        &[b"port-probe", b"snapshot", b"revoke"],
        &[b"port-probe", b"snapshot", b"grant", b"x"],
    ] {
        assert_eq!(run(arguments, &mut out), Err(CommandError::Usage));
    }
    assert_eq!(
        run(
            &[b"exhaustive-report", b"X\xff", b"0", b"pass", b"ok"],
            &mut out
        ),
        Err(CommandError::Usage)
    );
    assert_eq!(
        run(
            &[
                b"event", b"G_\xff", b"watcher", b"runtime", b"0", b"1", b"x"
            ],
            &mut out
        ),
        Err(CommandError::Usage)
    );
    assert!(out.is_empty());
    assert_eq!(run(&PAIR, &mut out), Ok(()));
    assert_eq!(out, b"theta_ns=1002000 epsilon_ns=1524\n");
}

#[test]
fn asynchronous_console_lines_are_written_whole() {
    let line = [b'a'; 700];
    let mut out = Vec::new();
    run(&[b"console", &line, b"async"], &mut out).unwrap();
    assert_eq!(out, [b"\n".as_slice(), &line].concat());
}

#[test]
fn console_lines_preserve_synchronous_and_asynchronous_boundaries() {
    let line = b"NVX-TIME-ABI-VIOLATION: v=1 code=G_RCU_STALL source=watcher\n";
    assert_eq!(hook(&[b"console", line, b"sync"]), line);
    assert_eq!(
        hook(&[b"console", line, b"async"]),
        [b"\n".as_slice(), line].concat()
    );
}

#[test]
fn exhaustive_hooks_preserve_leaf_rules_and_report_formats() {
    let out_of_range: [&[u8]; 4] = [b"0x00000d0b", b"0x00000001", b"0x00000002", b"0x00000003"];
    let zero: [&[u8]; 4] = [b"0", b"0", b"0", b"0"];
    let leaf = |number: &[u8], value: [&[u8]; 4]| {
        hook(&[
            b"exhaustive-leaf",
            number,
            value[0],
            value[1],
            value[2],
            value[3],
            out_of_range[0],
            out_of_range[1],
            out_of_range[2],
            out_of_range[3],
        ])
    };
    assert_eq!(leaf(b"0x40000006", zero), b"pass\n");
    assert_eq!(leaf(b"0x40000010", out_of_range), b"pass\n");
    assert_eq!(leaf(b"0x40000100", zero), b"pass\n");
    assert_eq!(leaf(b"0x40000100", out_of_range), b"pass\n");

    let explicit = String::from_utf8(leaf(b"0x40000081", out_of_range)).unwrap();
    assert!(
        explicit.starts_with("fail leaf 0x40000081 is "),
        "{explicit}"
    );
    assert!(explicit.ends_with("expected zero\n"), "{explicit}");
    let kvm = [
        b"0x40000101".as_slice(),
        b"0x4b4d564b",
        b"0x564b4d56",
        b"0x0000004d",
    ];
    let signature = String::from_utf8(leaf(b"0x40000100", kvm)).unwrap();
    assert!(
        signature.contains("(signature \"KVMKVMKVM...\")"),
        "{signature}"
    );
    assert!(
        signature.ends_with("or the highest basic leaf\n"),
        "{signature}"
    );

    assert_eq!(
        hook(&[b"exhaustive-report", b"X3", b"2", b"pass", b"ok"]),
        concat!(
            "NVX-TIME-ABI-EXHAUSTIVE: v=1 check=X3 cpu=2 status=pass detail=\"ok\"\n",
            "failures=0\n"
        )
        .as_bytes()
    );
    assert_eq!(
        hook(&[
            b"exhaustive-report",
            b"X2",
            b"7",
            b"fail",
            b"base 0x40000100 \"KVMKVMKVM\" \\ x\ty",
        ]),
        concat!(
            "NVX-TIME-ABI-EXHAUSTIVE: v=1 check=X2 cpu=7 status=fail ",
            "detail=\"base 0x40000100 \\\"KVMKVMKVM\\\" \\\\ x\\x09y\"\n",
            "failures=1\n"
        )
        .as_bytes()
    );
    assert_eq!(
        hook(&[b"exhaustive-summary", b"4", b"0"]),
        concat!(
            "NVX-TIME-ABI-EXHAUSTIVE: v=1 status=ok cpus=4 failures=0\n",
            "exit=0\n"
        )
        .as_bytes()
    );
    assert_eq!(
        hook(&[b"exhaustive-summary", b"8", b"3"]),
        concat!(
            "NVX-TIME-ABI-EXHAUSTIVE: v=1 status=fail cpus=8 failures=3\n",
            "exit=1\n"
        )
        .as_bytes()
    );
}

#[test]
fn accepted_cmdline_and_clocksource_hooks_print_ok() {
    assert_eq!(
        hook(&[
            b"cmdline",
            b"earlycon=xe9 console=hvc0 quiet clocksource=tsc tsc=reliable",
        ]),
        b"ok\n"
    );
    assert_eq!(hook(&[b"clocksource", b"tsc\n", b"tsc \n"]), b"ok\n");
}

#[test]
fn corrections_print_the_kernel_modes() {
    let mut out = Vec::new();
    run(
        &[b"correction", b"128000000", b"10", b"fast", b"0", b"0"],
        &mut out,
    )
    .unwrap();
    run(
        &[b"correction", b"-5", b"10", b"slow", b"65536", b"7"],
        &mut out,
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "step modes=0x2100 sec=0 nsec=128000000\n\
             slew modes=0x203f offset=-5 status=0x2081 constant=6 freq=65536 maxerror=8 \
             esterror=1\n"
    );
}

#[test]
fn ported_corrections_print_exact_results() {
    let mut out = Vec::new();
    for arguments in [
        [&b"127999999"[..], b"10002", b"fast"],
        [b"-500", b"20", b"slow"],
        [b"128000000", b"10", b"fast"],
        [b"-1128000000", b"10", b"slow"],
    ] {
        let hook = [&[&b"correction"[..]][..], &arguments, &[b"0", b"0"]].concat();
        run(&hook, &mut out).unwrap();
    }
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "slew modes=0x203f offset=127999999 status=0x2081 constant=4 freq=0 \
             maxerror=128011 esterror=11\n\
             slew modes=0x203f offset=-500 status=0x2081 constant=6 freq=0 maxerror=1 \
             esterror=1\n\
             step modes=0x2100 sec=0 nsec=128000000\n\
             step modes=0x2100 sec=-2 nsec=872000000\n"
    );
}

#[test]
fn pairing_and_frequency_hooks_preserve_signed_arithmetic() {
    let pair = |tsc: &[u8]| {
        hook(&[
            b"pair",
            b"1000000000000000000",
            tsc,
            b"500",
            b"999000",
            b"1002000",
            b"999999999999000000",
            b"1004000",
            b"40",
            b"2000000000",
            b"0",
        ])
    };
    assert_eq!(pair(b"1000000"), b"theta_ns=1002000 epsilon_ns=1524\n");
    assert_eq!(
        pair(b"995999"),
        b"error time sample TSC 995999 is outside the guest's reads 999000 to 1002000\n"
    );
    for (correction, tsc_hz, expected) in [
        (
            b"13107200".as_slice(),
            b"2100000000".as_slice(),
            b"frequency=13107200 ppb=200000 limited=0\n".as_slice(),
        ),
        (
            b"39321600",
            b"2100000000",
            b"frequency=32768000 ppb=500000 limited=1\n",
        ),
        (b"0", b"2194843123", b"frequency=-3673 ppb=-56 limited=0\n"),
        (
            b"-65536",
            b"2000000000",
            b"frequency=-65536 ppb=-1000 limited=0\n",
        ),
    ] {
        assert_eq!(hook(&[b"frequency", correction, tsc_hz]), expected);
    }
}
