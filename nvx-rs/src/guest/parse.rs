//! Strict parsers of the numbers in the guest programs' command-line arguments and in the
//! kernel's text files.
//!
//! The C programs parsed command-line arguments with `strtoull(text, &end, 0)`, which also
//! accepts leading whitespace, a sign, and octal after a leading zero. No caller passes those
//! forms, so the programs reject them as usage errors rather than read them differently. The
//! managed agent's C program read its identities and descriptor in base 10 instead, so they are
//! plain decimal.
//!
//! The kernel's text files, such as its socket tables and sysctls, hold plain decimal and
//! hexadecimal numbers, which the C programs read with the C library's `strtoul` and `scanf`.
//! Those also accept the forms above, and saturate an overflowing value, but the kernel never
//! writes them, so the programs parse the kernel's numbers as strictly as their arguments.

#[cfg(any(target_arch = "x86_64", test))]
use std::str::FromStr;

/// Parses a whole, nonempty decimal number, or a hexadecimal one after a `0x` or `0X` prefix.
///
/// Returns `None` for an empty argument, any other character, a decimal number with a leading
/// zero, which C read as octal, or a value above `u64::MAX`.
pub(crate) fn parse_u64(text: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(text).ok()?;
    let (digits, radix) = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(digits) => (digits, 16),
        None if text.len() > 1 && text.starts_with('0') => return None,
        None => (text, 10),
    };
    // `from_str_radix` alone would also accept a leading `+`.
    if digits.is_empty() || !digits.chars().all(|digit| digit.is_digit(radix)) {
        return None;
    }
    u64::from_str_radix(digits, radix).ok()
}

/// Parses a whole, nonempty decimal number, whose leading zeros are decimal, as C's `strtoull`
/// and `strtol` read one in base 10.
///
/// Returns `None` for an empty argument, any other character, including the leading whitespace
/// and sign that C also skipped, or a value above `u64::MAX`.
pub(crate) fn parse_decimal(text: &[u8]) -> Option<u64> {
    // `from_str` alone would also accept a leading `+`.
    if text.is_empty() || !text.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(text).ok()?.parse().ok()
}

/// Parses a whole, nonempty decimal integer, with a `-` sign when it is negative, into `T`, as
/// the time tools' test hooks read their numbers.
///
/// Returns `None` for any other form, including the leading whitespace and `+` sign that C's
/// `strtoll` also skipped, or for a value outside `T`, which it saturated.
#[cfg(any(target_arch = "x86_64", test))]
pub(crate) fn parse_integer<T: FromStr>(text: &[u8]) -> Option<T> {
    let digits = text.strip_prefix(b"-").unwrap_or(text);
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(text).ok()?.parse().ok()
}

/// Parses a whole, nonempty hexadecimal number without a prefix, in either case, as the kernel
/// prints one with `%X`.
///
/// Returns `None` for an empty text, any other character, including the leading whitespace, sign,
/// and `0x` prefix that C's `strtoul` and `scanf` also accepted, or a value above `u64::MAX`,
/// which they saturated.
pub(crate) fn parse_hexadecimal(text: &[u8]) -> Option<u64> {
    // `from_str_radix` alone would also accept a leading `+`.
    if text.is_empty() || !text.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    u64::from_str_radix(std::str::from_utf8(text).ok()?, 16).ok()
}

/// Parses a whole, finite decimal floating-point number: digits with an optional fraction, at
/// least one digit in all, and an optional exponent, such as `2.5`, `.5`, `1e9`, or `2.4e+9`.
///
/// Returns `None` for any other form, including the leading whitespace, sign, hexadecimal
/// significand, infinity, and NaN that C's `strtod` also read, or for a value too large for a
/// finite `f64`, which `strtod` reported as a range error.
#[cfg(any(target_arch = "x86_64", test))]
pub(crate) fn parse_f64(text: &[u8]) -> Option<f64> {
    let text = std::str::from_utf8(text).ok()?;
    let digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
    let (significand, exponent) = match text.split_once(['e', 'E']) {
        Some((significand, exponent)) => (significand, Some(exponent)),
        None => (text, None),
    };
    let (whole, fraction) = significand.split_once('.').unwrap_or((significand, ""));
    if whole.len() + fraction.len() == 0 || !digits(whole) || !digits(fraction) {
        return None;
    }
    if let Some(exponent) = exponent {
        let magnitude = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        if magnitude.is_empty() || !digits(magnitude) {
            return None;
        }
    }
    // `from_str` alone would also accept a sign, an infinity, and a NaN.
    text.parse().ok().filter(|value: &f64| value.is_finite())
}

#[cfg(test)]
mod tests;
