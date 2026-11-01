use super::*;

#[test]
fn formats_with_c_rounding_and_spelling() {
    assert_eq!(f0(12.5), "12");
    assert_eq!(f0(13.5), "14");
    assert_eq!(f1(12.25), "12.2");
    assert_eq!(f9(1.5), "1.500000000");
    assert_eq!(f1(-0.0), "-0.0");
    assert_eq!(f0(f64::INFINITY), "inf");
    assert_eq!(f1(f64::NEG_INFINITY), "-inf");
    assert_eq!(f9(f64::NAN), "nan");
    assert_eq!(f0(-f64::NAN), "-nan");
}

#[test]
fn formats_the_complete_finite_double_range_without_truncation() {
    let integer = f0(f64::MAX);
    assert_eq!(integer.len(), 309);
    assert!(integer.starts_with("179769313486231570814527423731704356798070567525"));
    assert!(integer.ends_with("50404026184124858368"));

    let one_decimal = f1(f64::MAX);
    assert_eq!(one_decimal.len(), 311);
    assert!(one_decimal.ends_with(".0"));

    let nine_decimals = f9(f64::MAX);
    assert_eq!(nine_decimals.len(), 319);
    assert!(nine_decimals.ends_with(".000000000"));
}

#[test]
fn utc_times_follow_the_gregorian_calendar() {
    assert_eq!(utc_seconds(0), "1970-01-01T00:00:00");
    assert_eq!(utc_seconds(-1), "1969-12-31T23:59:59");
    assert_eq!(utc_seconds(951_782_400), "2000-02-29T00:00:00");
    assert_eq!(utc_seconds(1_790_841_600), "2026-10-01T08:00:00");
    assert_eq!(utc_seconds(4_107_542_399), "2100-02-28T23:59:59");
    assert_eq!(utc_seconds(4_107_542_400), "2100-03-01T00:00:00");
    assert_eq!(utc_seconds(253_402_300_799), "9999-12-31T23:59:59");
}
