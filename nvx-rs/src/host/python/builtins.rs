//! Python's built-in string and integer operations.
//!
//! - `is_space`: `str.isspace` for the CLI; test-only `split` and `strip` retain their oracles.
//!   Python's whitespace includes the ASCII separators U+001C through U+001F, which Rust's
//!   [`char::is_whitespace`] does not.
//! - [`repr`]: `repr` of a `str`, with which messages quote values.
//! - `Int`: a CLI-only Python `int` of any size, from `int(text)`, with its acceptance of signs,
//!   underscores, Unicode decimal digits, and surrounding whitespace, and its limit of
//!   `MAX_INT_DIGITS` decimal digits.

#[cfg(any(test, feature = "cli"))]
use std::cmp::Ordering;
#[cfg(any(test, feature = "cli"))]
use std::fmt;
use std::fmt::Write as _;

use super::unicode;

/// The most decimal digits that `int(text)` converts: `sys.get_int_max_str_digits()`.
#[cfg(any(test, feature = "cli"))]
pub(crate) const MAX_INT_DIGITS: usize = 4300;

/// Returns whether Python's `str.isspace` holds for `character`.
#[cfg(any(test, feature = "cli"))]
pub(crate) fn is_space(character: char) -> bool {
    matches!(
        u32::from(character),
        0x09..=0x0D
            | 0x1C..=0x20
            | 0x85
            | 0xA0
            | 0x1680
            | 0x2000..=0x200A
            | 0x2028
            | 0x2029
            | 0x202F
            | 0x205F
            | 0x3000
    )
}

/// Returns whether `str.isprintable` holds for `character`, so that `repr` shows it as is.
pub(crate) fn is_printable(character: char) -> bool {
    let code = u32::from(character);
    if code < 0x80 {
        return (0x20..0x7F).contains(&code);
    }
    let ranges = unicode::NON_PRINTABLE;
    // The ranges are sorted and disjoint: find the last one that starts at or before `code`.
    let index = ranges.partition_point(|&(start, _)| start <= code);
    index == 0 || ranges[index - 1].1 < code
}

/// Returns the value of a Unicode decimal digit, of category Nd, as `int` reads it.
#[cfg(any(test, feature = "cli"))]
pub(crate) fn decimal_value(character: char) -> Option<u32> {
    let code = u32::from(character);
    let starts = unicode::DECIMAL_STARTS;
    // Every run of decimal digits is ten consecutive code points, from zero to nine.
    let index = starts.partition_point(|&start| start <= code);
    let start = *starts.get(index.checked_sub(1)?)?;
    (code - start < 10).then_some(code - start)
}

/// Splits `text` at runs of Python whitespace and drops empty fields, as `str.split()` does.
#[cfg(test)]
pub(crate) fn split(text: &str) -> impl Iterator<Item = &str> {
    text.split(is_space).filter(|field| !field.is_empty())
}

/// Strips Python whitespace from both ends of `text`, as `str.strip()` does.
#[cfg(test)]
pub(crate) fn strip(text: &str) -> &str {
    text.trim_matches(is_space)
}

/// Returns `repr(text)`: `text` in quotes, with backslashes, the quote, and the characters that
/// are not printable escaped.
pub(crate) fn repr(text: &str) -> String {
    // Single quotes, unless the text holds single quotes and no double quotes.
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push(quote);
    for character in text.chars() {
        let code = u32::from(character);
        match character {
            _ if character == quote || character == '\\' => {
                quoted.push('\\');
                quoted.push(character);
            }
            '\t' => quoted.push_str("\\t"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            _ if code < 0x20 || code == 0x7F => {
                let _ = write!(quoted, "\\x{code:02x}");
            }
            _ if code < 0x7F || is_printable(character) => quoted.push(character),
            _ if code <= 0xFF => {
                let _ = write!(quoted, "\\x{code:02x}");
            }
            _ if code <= 0xFFFF => {
                let _ = write!(quoted, "\\u{code:04x}");
            }
            _ => {
                let _ = write!(quoted, "\\U{code:08x}");
            }
        }
    }
    quoted.push(quote);
    quoted
}

/// Why `int(text)` refuses `text`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(any(test, feature = "cli"))]
pub(crate) enum IntError {
    /// The text is not an integer literal.
    Invalid,
    /// The literal has this many decimal digits, more than `int` converts (4300).
    TooManyDigits(usize),
}

#[cfg(any(test, feature = "cli"))]
impl IntError {
    /// Returns the message of the `ValueError` with which `int(text)` refuses `text`.
    pub(crate) fn message(&self, text: &str) -> String {
        match self {
            Self::Invalid => {
                // `%.200R` keeps the first 200 characters of the representation.
                let quoted: String = repr(text).chars().take(200).collect();
                format!("invalid literal for int() with base 10: {quoted}")
            }
            Self::TooManyDigits(digits) => too_many_digits_message(*digits),
        }
    }
}

/// Returns the message of the `ValueError` that refuses a decimal literal of `digits` digits,
/// more than [`MAX_INT_DIGITS`].
#[cfg(any(test, feature = "cli"))]
pub(crate) fn too_many_digits_message(digits: usize) -> String {
    format!(
        "Exceeds the limit ({MAX_INT_DIGITS} digits) for integer string conversion: value has \
         {digits} digits; use sys.set_int_max_str_digits() to increase the limit"
    )
}

/// A Python `int` of any size.
///
/// [`Display`](fmt::Display) gives its decimal form, as `str()` does.
#[derive(Clone, PartialEq, Eq, Hash)]
#[cfg(any(test, feature = "cli"))]
pub(crate) struct Int {
    /// Whether the integer is below zero; zero is never negative.
    negative: bool,
    /// The decimal digits of the magnitude, without leading zeros: `"0"` for zero.
    digits: String,
}

#[cfg(any(test, feature = "cli"))]
impl Int {
    /// Zero.
    pub(crate) fn zero() -> Self {
        Self {
            negative: false,
            digits: "0".to_owned(),
        }
    }

    /// Parses `text` as `int(text)` does.
    ///
    /// Python first maps Unicode whitespace to spaces and Unicode decimal digits to ASCII
    /// digits, and refuses any other character outside ASCII. It then accepts surrounding ASCII
    /// whitespace, a sign, leading zeros, and single underscores between digits.
    pub(crate) fn parse(text: &str) -> Result<Self, IntError> {
        let (negative, digits) = parse_digits(text, 10)?;
        Ok(Self::from_digits(negative, &digits))
    }

    /// Builds the integer of decimal `digits`, most significant first.
    fn from_digits(negative: bool, digits: &[u8]) -> Self {
        let significant = digits
            .iter()
            .position(|&digit| digit != 0)
            .map_or(&[][..], |first| &digits[first..]);
        if significant.is_empty() {
            return Self::zero();
        }
        Self {
            negative,
            digits: significant
                .iter()
                .map(|&digit| char::from(b'0' + digit))
                .collect(),
        }
    }

    /// Returns the integer as a `u32`, if it is one.
    pub(crate) fn to_u32(&self) -> Option<u32> {
        self.to_u64().and_then(|value| u32::try_from(value).ok())
    }

    /// Returns the integer as a `u64`, if it is one.
    pub(crate) fn to_u64(&self) -> Option<u64> {
        if self.negative {
            return None;
        }
        self.digits.parse().ok()
    }
}

#[cfg(any(test, feature = "cli"))]
impl From<u64> for Int {
    fn from(value: u64) -> Self {
        Self {
            negative: false,
            digits: value.to_string(),
        }
    }
}

#[cfg(any(test, feature = "cli"))]
impl From<u32> for Int {
    fn from(value: u32) -> Self {
        Self::from(u64::from(value))
    }
}

#[cfg(any(test, feature = "cli"))]
impl From<i64> for Int {
    fn from(value: i64) -> Self {
        Self {
            negative: value < 0,
            digits: value.unsigned_abs().to_string(),
        }
    }
}

#[cfg(any(test, feature = "cli"))]
impl PartialOrd for Int {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(any(test, feature = "cli"))]
impl Ord for Int {
    fn cmp(&self, other: &Self) -> Ordering {
        let magnitude = || {
            self.digits
                .len()
                .cmp(&other.digits.len())
                .then_with(|| self.digits.cmp(&other.digits))
        };
        match (self.negative, other.negative) {
            (false, false) => magnitude(),
            (true, true) => magnitude().reverse(),
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
        }
    }
}

#[cfg(any(test, feature = "cli"))]
impl fmt::Display for Int {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.negative {
            formatter.write_str("-")?;
        }
        formatter.write_str(&self.digits)
    }
}

#[cfg(any(test, feature = "cli"))]
impl fmt::Debug for Int {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

/// Parses `text` as `int(text, base)` does for base 10 or 16, returning its sign and its digit
/// values, most significant first and with any leading zeros. See [`Int::parse`]; base 16 also
/// accepts a `0x` prefix, which a single underscore may follow.
#[cfg(any(test, feature = "cli"))]
pub(crate) fn parse_digits(text: &str, base: u32) -> Result<(bool, Vec<u8>), IntError> {
    let mut ascii = Vec::with_capacity(text.len());
    for character in text.chars() {
        let code = u32::from(character);
        if code < 0x7F {
            ascii.push(code as u8);
        } else if is_space(character) {
            ascii.push(b' ');
        } else if let Some(value) = decimal_value(character) {
            ascii.push(b'0' + value as u8);
        } else {
            return Err(IntError::Invalid);
        }
    }
    let is_ascii_space = |byte: u8| matches!(byte, b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r');
    let mut index = 0;
    while ascii.get(index).copied().is_some_and(is_ascii_space) {
        index += 1;
    }
    let mut negative = false;
    match ascii.get(index) {
        Some(b'+') => index += 1,
        Some(b'-') => {
            negative = true;
            index += 1;
        }
        _ => {}
    }
    if base == 16
        && ascii.get(index) == Some(&b'0')
        && matches!(ascii.get(index + 1), Some(b'x' | b'X'))
    {
        index += 2;
        if ascii.get(index) == Some(&b'_') {
            index += 1;
        }
    }
    if ascii.get(index) == Some(&b'_') {
        return Err(IntError::Invalid);
    }
    let mut digits = Vec::new();
    let mut previous = 0;
    while let Some(&byte) = ascii.get(index) {
        if byte == b'_' {
            if previous == b'_' {
                return Err(IntError::Invalid);
            }
        } else {
            match char::from(byte).to_digit(base) {
                Some(value) => digits.push(value as u8),
                None => break,
            }
        }
        previous = byte;
        index += 1;
    }
    if previous == b'_' || digits.is_empty() {
        return Err(IntError::Invalid);
    }
    while ascii.get(index).copied().is_some_and(is_ascii_space) {
        index += 1;
    }
    // Anything else after the digits, including a NUL, makes the literal invalid.
    if index != ascii.len() {
        return Err(IntError::Invalid);
    }
    if base == 10 && digits.len() > MAX_INT_DIGITS {
        return Err(IntError::TooManyDigits(digits.len()));
    }
    Ok((negative, digits))
}

/// Parses `text` as `float(text)` does, or returns `None` where it raises `ValueError`.
///
/// Python maps Unicode whitespace to spaces and Unicode decimal digits to ASCII digits, and
/// refuses any other character outside ASCII. Single underscores may separate digits. It then
/// strips surrounding ASCII whitespace and reads a decimal number with an optional exponent, or
/// `inf`, `infinity`, or `nan` in any case, each with an optional sign; numbers beyond the range
/// of a float become infinities.
#[cfg(feature = "cli")]
pub(crate) fn parse_float(text: &str) -> Option<f64> {
    let mut ascii = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_ascii() {
            ascii.push(character);
        } else if is_space(character) {
            ascii.push(' ');
        } else {
            ascii.push(char::from(b'0' + decimal_value(character)? as u8));
        }
    }
    let mut digits = String::with_capacity(ascii.len());
    let mut previous = '\0';
    for character in ascii.chars() {
        if character == '_' {
            if !previous.is_ascii_digit() {
                return None;
            }
        } else {
            if previous == '_' && !character.is_ascii_digit() {
                return None;
            }
            digits.push(character);
        }
        previous = character;
    }
    if previous == '_' {
        return None;
    }
    let number = digits.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r'));
    // Rust reads the grammar of `PyOS_string_to_double`: digits with an optional fraction and
    // exponent, or `inf`, `infinity`, and `nan` in any case, after an optional sign.
    number.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_whitespace_includes_the_ascii_separators() {
        for space in [
            '\t', '\n', '\x0b', '\x0c', '\r', '\x1c', '\x1f', ' ', '\u{85}', '\u{3000}',
        ] {
            assert!(is_space(space), "{space:?}");
        }
        for other in ['a', '\0', '\u{200b}', '\u{feff}', '\u{180e}'] {
            assert!(!is_space(other), "{other:?}");
        }
        assert_eq!(
            split(" a\x1cb  c\u{3000}").collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert_eq!(strip("\x1f a b \u{85}"), "a b");
    }

    #[test]
    fn repr_quotes_and_escapes_as_python() {
        for (text, expected) in [
            ("abc", "'abc'"),
            ("it's", "\"it's\""),
            ("say \"hi\"", "'say \"hi\"'"),
            ("both ' and \"", "'both \\' and \"'"),
            ("a\\b", "'a\\\\b'"),
            ("\t\n\r\0\x7f\x1b", "'\\t\\n\\r\\x00\\x7f\\x1b'"),
            ("é\u{a0}\u{ad}", "'é\\xa0\\xad'"),
            ("\u{2028}\u{200b}\u{378}", "'\\u2028\\u200b\\u0378'"),
            ("\u{1f600}\u{e0001}", "'\u{1f600}\\U000e0001'"),
        ] {
            assert_eq!(repr(text), expected, "{text:?}");
        }
    }

    #[test]
    fn integers_parse_as_python_int() {
        for (text, expected) in [
            ("007", "7"),
            ("+5", "5"),
            ("-5", "-5"),
            (" 5 ", "5"),
            ("5_000", "5000"),
            ("\u{2003}5\u{3000}", "5"),
            ("\u{ff15}", "5"),
            ("\u{664}\u{662}", "42"),
            ("4\u{660}2", "402"),
            ("-0", "0"),
            ("\x0b42\x0c", "42"),
        ] {
            assert_eq!(Int::parse(text).unwrap().to_string(), expected, "{text:?}");
        }
        for text in [
            "",
            " ",
            "+",
            "-",
            "+-5",
            "_5",
            "5_",
            "5__0",
            "0x5",
            "0d5",
            "1e3",
            "5.0",
            "4\u{200b}2",
            "\x1c42\x1f",
            "1\x002",
            "\u{7f}1",
            "5 5",
        ] {
            assert_eq!(Int::parse(text), Err(IntError::Invalid), "{text:?}");
        }
        assert!(Int::parse(&"1".repeat(MAX_INT_DIGITS)).is_ok());
        // Leading zeros count as digits.
        assert_eq!(
            Int::parse(&format!("-{}1", "0".repeat(MAX_INT_DIGITS))),
            Err(IntError::TooManyDigits(MAX_INT_DIGITS + 1))
        );
        assert_eq!(
            IntError::TooManyDigits(4301).message(""),
            "Exceeds the limit (4300 digits) for integer string conversion: value has 4301 \
             digits; use sys.set_int_max_str_digits() to increase the limit"
        );
        assert_eq!(
            IntError::Invalid.message("x'y"),
            "invalid literal for int() with base 10: \"x'y\""
        );
    }

    #[test]
    fn integers_compare_and_convert_at_any_size() {
        let parse = |text: &str| Int::parse(text).unwrap();
        assert_eq!(parse("4294967295").to_u32(), Some(u32::MAX));
        assert_eq!(parse("4294967296").to_u32(), None);
        assert_eq!(parse("-1").to_u64(), None);
        assert!(parse("-3") < parse("-2"));
        assert!(parse("-2") < Int::zero());
        assert!(parse("10") > parse("9"));
        assert_eq!(Int::from(-7_i64).to_string(), "-7");
    }

    #[test]
    #[cfg(feature = "cli")]
    fn floats_parse_as_python_float() {
        for (text, expected) in [
            ("60", 60.0),
            (" 0.25 ", 0.25),
            ("1_000.5", 1000.5),
            ("1e1_0", 1e10),
            (".5", 0.5),
            ("5.", 5.0),
            ("1.e2", 100.0),
            ("+1E-2", 0.01),
            ("\u{664}\u{662}", 42.0),
            ("\u{3000}7\u{85}", 7.0),
            ("1e400", f64::INFINITY),
            ("-Infinity", f64::NEG_INFINITY),
            ("iNf", f64::INFINITY),
        ] {
            assert_eq!(parse_float(text), Some(expected), "{text:?}");
        }
        assert!(parse_float("-nan").is_some_and(f64::is_nan));
        for text in [
            "",
            " ",
            ".",
            "e5",
            "1e",
            "_1",
            "1_",
            "1__0",
            "1_.5",
            "1._5",
            "inf_",
            "0x10",
            "1 2",
            "nan(1)",
            "in",
            "\u{200b}1",
            "1\0",
            "+",
            "--1",
        ] {
            assert_eq!(parse_float(text), None, "{text:?}");
        }
    }

    #[test]
    fn unicode_tables_classify_digits_and_printable_characters() {
        assert_eq!(decimal_value('7'), Some(7));
        assert_eq!(decimal_value('\u{663}'), Some(3));
        assert_eq!(decimal_value('\u{ff19}'), Some(9));
        assert_eq!(decimal_value('\u{2155}'), None);
        assert_eq!(decimal_value('a'), None);
        assert!(is_printable('é'));
        assert!(!is_printable('\u{a0}'));
        assert!(!is_printable('\u{10ffff}'));
    }
}
