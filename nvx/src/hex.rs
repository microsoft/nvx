//! Hexadecimal text: the one definition, shared by the host, the guest, and the protocol, of bytes
//! as lowercase digits, two for each byte and without a separator, and of such digits, in either
//! case, as bytes.

use std::fmt;

/// Writes `bytes` as lowercase hexadecimal digits, two for each byte.
pub(crate) fn write_hex(formatter: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
    bytes
        .iter()
        .try_for_each(|byte| write!(formatter, "{byte:02x}"))
}

/// Returns `bytes` as lowercase hexadecimal digits, two for each byte.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Parses exactly `2 * N` hexadecimal digits, in either case, into the `N` bytes that they spell.
///
/// A sign, whitespace, and any other byte that is not a digit, ASCII or not, refuses the text.
pub(crate) fn parse_hex<const N: usize>(text: impl AsRef<[u8]>) -> Option<[u8; N]> {
    let digits = text.as_ref();
    if digits.len() != 2 * N {
        return None;
    }
    let mut bytes = [0; N];
    for (byte, pair) in bytes.iter_mut().zip(digits.chunks_exact(2)) {
        let nibble = |digit: u8| char::from(digit).to_digit(16);
        *byte = u8::try_from(nibble(pair[0])? * 16 + nibble(pair[1])?).ok()?;
    }
    Some(bytes)
}

#[cfg(test)]
mod tests;
