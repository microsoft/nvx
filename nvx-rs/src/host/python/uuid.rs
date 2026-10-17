//! Python's `uuid.UUID` parser.

use super::builtins::parse_digits;

/// Returns `str(uuid.UUID(text))`, the lowercase hyphenated form of the UUID in `text`, or `None`
/// if `uuid.UUID` refuses `text`.
///
/// `uuid.UUID` removes every `urn:` and then every `uuid:`, strips braces from both ends, removes
/// every hyphen, and requires 32 characters to remain, which it reads with `int(text, 16)`. It
/// therefore accepts braces, a `urn:uuid:` prefix, the form without hyphens, and uppercase
/// digits, and also Unicode decimal digits, a `0x` prefix, a plus sign, underscores, and
/// surrounding whitespace, as long as 32 characters remain. A minus sign is a hyphen, which is
/// removed.
pub(crate) fn parse_uuid(text: &str) -> Option<String> {
    let hex = text.replace("urn:", "").replace("uuid:", "");
    let hex = hex.trim_matches(['{', '}']).replace('-', "");
    if hex.chars().count() != 32 {
        return None;
    }
    let (_, digits) = parse_digits(&hex, 16).ok()?;
    // At most 32 hexadecimal digits remain, so the value fits in 128 bits.
    let value = digits
        .iter()
        .fold(0u128, |value, &digit| (value << 4) | u128::from(digit));
    let hex = format!("{value:032x}");
    Some(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_take_every_form_that_python_accepts() {
        let canonical = "12345678-1234-5678-1234-567812345678";
        for text in [
            "12345678-1234-5678-1234-567812345678",
            "{12345678-1234-5678-1234-567812345678}",
            "urn:uuid:12345678-1234-5678-1234-567812345678",
            "12345678123456781234567812345678",
            "12345678-1234-5678-1234-567812345678"
                .to_uppercase()
                .as_str(),
            "{{urn:12345678-1234-5678-1234-567812345678}}",
            "{urn:uuid:12345678-1234-5678-1234-567812345678}",
        ] {
            assert_eq!(parse_uuid(text).as_deref(), Some(canonical), "{text:?}");
        }
        // Each prefix is removed in one pass, as `str.replace` removes it.
        assert_eq!(
            parse_uuid("uuuuid:id:12345678123456781234567812345678"),
            None
        );
        // `int(text, 16)` reads a prefix, whitespace, a sign, and Unicode digits too.
        for (text, expected) in [
            (
                format!("0x{}", "1".repeat(30)),
                "00111111-1111-1111-1111-111111111111",
            ),
            (
                format!(" {}", "a".repeat(31)),
                "0aaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            ),
            (
                format!("+{}", "1".repeat(31)),
                "01111111-1111-1111-1111-111111111111",
            ),
            (
                format!("1_{}", "1".repeat(30)),
                "01111111-1111-1111-1111-111111111111",
            ),
            (
                format!("\u{661}{}", "0".repeat(31)),
                "10000000-0000-0000-0000-000000000000",
            ),
        ] {
            assert_eq!(parse_uuid(&text).as_deref(), Some(expected), "{text:?}");
        }
        for text in [
            "not-a-uuid".to_owned(),
            String::new(),
            // Hyphens are removed, a minus sign with them.
            format!("-{}", "0".repeat(31)),
            format!("{}g", "1".repeat(31)),
            "1".repeat(33),
            format!("{}_", "1".repeat(31)),
        ] {
            assert_eq!(parse_uuid(&text), None, "{text:?}");
        }
    }
}
