use super::*;

fn parse(text: &[u8]) -> Option<u64> {
    parse_u64(text)
}

#[test]
fn parses_decimal_and_hexadecimal_numbers() {
    let cases: &[(&[u8], u64)] = &[
        (b"0", 0),
        (b"7", 7),
        (b"4096", 4096),
        (b"18446744073709551615", u64::MAX),
        (b"0x0", 0),
        (b"0x10", 16),
        (b"0XfF", 255),
        (b"0x00ff", 255),
        (b"0xffffffffffffffff", u64::MAX),
    ];
    for &(text, expected) in cases {
        assert_eq!(parse(text), Some(expected), "{}", text.escape_ascii());
    }
}

#[test]
fn rejects_every_other_form() {
    for text in [
        &b""[..],
        b"0x",
        b"0X",
        b"00",
        b"010",
        b"08",
        b"+1",
        b"-1",
        b" 1",
        b"1 ",
        b"1a",
        b"0x1g",
        b"0x+1",
        b"0x-1",
        b"1_000",
        b"0b1",
        b"18446744073709551616",
        b"0x10000000000000000",
        b"\xa01",
        "\u{661}".as_bytes(),
    ] {
        assert_eq!(parse(text), None, "{}", text.escape_ascii());
    }
}

#[test]
fn parses_plain_decimal_numbers_with_leading_zeros() {
    let cases: &[(&[u8], u64)] = &[
        (b"0", 0),
        (b"7", 7),
        (b"007", 7),
        (b"2147483647", 2_147_483_647),
        (b"18446744073709551615", u64::MAX),
    ];
    for &(text, expected) in cases {
        assert_eq!(
            parse_decimal(text),
            Some(expected),
            "{}",
            text.escape_ascii()
        );
    }
    for text in [
        &b""[..],
        b"+7",
        b"-1",
        b" 42",
        b"42 ",
        b"1x",
        b"0x10",
        b"18446744073709551616",
        "\u{661}".as_bytes(),
    ] {
        assert_eq!(parse_decimal(text), None, "{}", text.escape_ascii());
    }
}

#[test]
fn parses_plain_signed_integers_into_their_type() {
    assert_eq!(parse_integer::<i64>(b"-1128000000"), Some(-1_128_000_000));
    assert_eq!(
        parse_integer::<i64>(b"-9223372036854775808"),
        Some(i64::MIN)
    );
    assert_eq!(parse_integer::<u32>(b"4294967295"), Some(u32::MAX));
    assert_eq!(parse_integer::<i32>(b"007"), Some(7));
    for text in [
        &b""[..],
        b"-",
        b"+7",
        b"--7",
        b" 7",
        b"7 ",
        b"0x10",
        b"9223372036854775808",
    ] {
        assert_eq!(parse_integer::<i64>(text), None, "{}", text.escape_ascii());
    }
    assert_eq!(parse_integer::<u64>(b"-1"), None);
    assert_eq!(parse_integer::<u32>(b"4294967296"), None);
}

#[test]
fn parses_plain_hexadecimal_numbers_in_either_case() {
    let cases: &[(&[u8], u64)] = &[
        (b"0", 0),
        (b"0A", 0x0a),
        (b"1f90", 0x1f90),
        (b"0100007F", 0x0100_007f),
        (b"FFFFFFFFFFFFFFFF", u64::MAX),
        (b"0000000000000000000001", 1),
    ];
    for &(text, expected) in cases {
        assert_eq!(
            parse_hexadecimal(text),
            Some(expected),
            "{}",
            text.escape_ascii()
        );
    }
    for text in [
        &b""[..],
        b"+A",
        b"-1",
        b"-0000001",
        b"0x10",
        b"0X10",
        b" A",
        b"A ",
        b"\x0bA",
        b"A\0",
        b"1F90:",
        b"g",
        b"10000000000000000",
        "\u{ff21}".as_bytes(),
    ] {
        assert_eq!(parse_hexadecimal(text), None, "{}", text.escape_ascii());
    }
}

#[test]
fn parses_finite_decimal_floating_point_numbers() {
    let cases: &[(&[u8], f64)] = &[
        (b"0", 0.0),
        (b"2.5", 2.5),
        (b".5", 0.5),
        (b"7.", 7.0),
        (b"1e9", 1e9),
        (b"2994.374E6", 2_994_374_000.0),
        (b"2.4e+9", 2.4e9),
        (b"5e-1", 0.5),
        (b"1e200", 1e200),
    ];
    for &(text, expected) in cases {
        assert_eq!(parse_f64(text), Some(expected), "{}", text.escape_ascii());
    }
    for text in [
        &b""[..],
        b".",
        b"e5",
        b"1e",
        b"1e+",
        b"+1",
        b"-1",
        b" 1",
        b"1 ",
        b"1.5x",
        b"0x1p3",
        b"inf",
        b"NaN",
        b"1e400",
        b"1.2.3",
    ] {
        assert_eq!(parse_f64(text), None, "{}", text.escape_ascii());
    }
}
