use super::*;
use crate::guest::parse::parse_u64;

#[test]
fn cpu_lists_reject_duplicates_and_format_ranges() {
    assert_eq!(parse_cpu_list(b"0-3,6"), Some(vec![0, 1, 2, 3, 6]));
    assert_eq!(parse_cpu_list(b""), Some(Vec::new()));
    assert_eq!(format_cpu_list(&[0, 1, 2, 3, 6]), "0-3,6");
    assert_eq!(format_cpu_list(&[6, 0, 1]), "6,0-1");
    assert_eq!(format_cpu_list(&[]), "");
    for list in [
        &b"0,0"[..],
        b"3-1",
        b"1024",
        b"0,",
        b",0",
        b"-1",
        b"+1",
        b" 1",
        b"1-",
        b"0x1",
    ] {
        assert!(parse_cpu_list(list).is_none(), "{}", list.escape_ascii());
    }
}

#[test]
fn numeric_options_are_strict() {
    assert_eq!(parse_u64(b"0x10"), Some(16));
    assert_eq!(parse_u64(b"10"), Some(10));
    assert!(parse_u64(b"010").is_none());
    assert!(parse_u64(b"1x").is_none());
    #[cfg(target_arch = "x86_64")]
    {
        assert_eq!(parse_rate(b"2.5"), Some(2.5));
        assert_eq!(parse_rate(b"1e200"), Some(1e200));
        assert!(parse_rate(b"0").is_none());
        assert!(parse_rate(b"2.5x").is_none());
    }
}

/// Returns the diagnostic that `arguments` prints for an invalid option or value.
fn diagnostic(arguments: &Arguments<'_>) -> Vec<u8> {
    let mut err = Vec::new();
    assert_eq!(arguments.invalid(&mut err), CommandError::Usage);
    err
}

#[test]
fn options_take_inline_or_following_values() {
    let arguments: [&[u8]; 4] = [b"--batch=5", b"--cpu", b"3", b"--cpu"];
    let mut arguments = Arguments::new(b"latency", &arguments);
    let first = arguments.next().unwrap();
    assert_eq!(arguments.value(first, b"--cpu"), None);
    assert_eq!(arguments.value(first, b"--batch"), Some(&b"5"[..]));
    let second = arguments.next().unwrap();
    assert_eq!(arguments.value(second, b"--cpu"), Some(&b"3"[..]));
    assert_eq!(
        diagnostic(&arguments),
        b"nvx-time-probe latency: invalid option or value: 3\n"
    );
    let third = arguments.next().unwrap();
    assert_eq!(arguments.value(third, b"--cpu"), None);
    assert_eq!(
        diagnostic(&arguments),
        b"nvx-time-probe latency: invalid option or value: --cpu\n"
    );
    assert_eq!(arguments.next(), None);
}

#[test]
#[cfg(target_arch = "x86_64")]
fn kernel_numbers_are_read_in_the_kernels_form() {
    assert_eq!(parse_kernel_decimal(b" 2194.843"), Some(2194.843));
    assert_eq!(parse_kernel_decimal(b"    3.000000"), Some(3.0));
    assert_eq!(parse_kernel_decimal(b"\t2194"), Some(2194.0));
    assert_eq!(parse_kernel_decimal(b"-1.5"), None);
    assert_eq!(parse_kernel_decimal(b"1e3"), None);
    assert_eq!(parse_kernel_decimal(b"2194.843 "), None);
    assert_eq!(
        mhz_after(
            b"tsc: Detected 2194.843 MHz processor",
            b"tsc: Detected ",
            b" MHz processor"
        ),
        Some(2194.843)
    );
    assert_eq!(
        mhz_after(
            b"tsc: Detected 2194.843 MHz TSC",
            b"tsc: Detected ",
            b" MHz processor"
        ),
        None
    );
    assert_eq!(
        mhz_after(b"tsc: Detected 0 MHz TSC", b"tsc: Detected ", b" MHz TSC"),
        None
    );
    assert_eq!(mhz_to_hz(2194.843), 2_194_843_000.0);
}

#[test]
#[cfg(target_arch = "x86_64")]
fn rate_sources_print_their_tokens() {
    assert_eq!(
        TscRateSource::SysfsTscFreqKhz.to_string(),
        "sysfs-tsc_freq_khz"
    );
    let rate = detect_tsc_rate(Some(2.5e9));
    assert_eq!((rate.hz, rate.source), (2.5e9, TscRateSource::Option));
}

#[test]
fn nearest_integer_rounding_matches_the_probe() {
    assert_eq!(round_ll(1.49), 1);
    assert_eq!(round_ll(1.5), 2);
    assert_eq!(round_ll(-1.5), -2);
    assert_eq!(round_ll(-0.4), 0);
}
