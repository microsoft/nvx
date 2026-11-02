//! UUIDs: the 128-bit identifiers of, for example, a GPT disk and its partitions, which GPT calls
//! GUIDs, and of filesystems.
//!
//! A [`Uuid`] has one text form, its canonical text: 32 lowercase hexadecimal digits in groups of
//! 8, 4, 4, 4, and 12, joined by hyphens, as in `0fc63daf-8483-4772-8e79-3d69d8477de4`. It
//! displays in that form and parses from it alone, so that each UUID has exactly one text. A
//! caller that accepts other forms, such as uppercase digits or braces, normalizes them before it
//! parses them.
//!
//! Its bytes are those of its digits, in the order of the text, as RFC 9562 orders them and as
//! filesystems such as EROFS and ext4 store their UUIDs. GPT, like other EFI GUIDs, stores the
//! first three groups little-endian instead, which [`Uuid::from_bytes_le`] and
//! [`Uuid::to_bytes_le`] convert.
//!
//! The module has no dependencies, so the host and the guest share it.

use std::fmt;
use std::str::FromStr;

/// The offsets of the hyphens in the canonical text of a UUID.
const HYPHENS: [usize; 4] = [8, 13, 18, 23];

/// The length of the canonical text of a UUID.
const TEXT_LEN: usize = 36;

/// A UUID, whose 16 bytes are in the order of its canonical text, as the module describes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Uuid([u8; 16]);

impl Uuid {
    /// The nil UUID, all of whose bits are zero, which marks, for example, an unused GPT partition
    /// entry.
    pub const NIL: Self = Self([0; 16]);

    /// Returns the UUID whose bytes, in the order of its text, are `bytes`.
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Returns the UUID's bytes, in the order of its text.
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Returns the UUID that `bytes` hold in the layout of GPT and other EFI GUIDs, whose first
    /// three groups are little-endian.
    pub const fn from_bytes_le(bytes: [u8; 16]) -> Self {
        Self(swap_groups(bytes))
    }

    /// Returns the UUID's bytes in the layout of GPT and other EFI GUIDs, whose first three groups
    /// are little-endian, as [`Uuid::from_bytes_le`] reads them.
    pub const fn to_bytes_le(&self) -> [u8; 16] {
        swap_groups(self.0)
    }

    /// Returns whether this is the nil UUID.
    pub fn is_nil(&self) -> bool {
        *self == Self::NIL
    }

    /// Parses the canonical text of a UUID, and no other form: not uppercase digits, braces, a
    /// prefix, or other groups.
    pub fn parse(text: impl AsRef<[u8]>) -> Option<Self> {
        let text = text.as_ref();
        if text.len() != TEXT_LEN || HYPHENS.iter().any(|&offset| text[offset] != b'-') {
            return None;
        }
        let mut digits = text
            .iter()
            .enumerate()
            .filter(|(offset, _)| !HYPHENS.contains(offset))
            .map(|(_, &digit)| match digit {
                b'0'..=b'9' => Some(digit - b'0'),
                b'a'..=b'f' => Some(digit - b'a' + 10),
                _ => None,
            });
        let mut bytes = [0; 16];
        for byte in &mut bytes {
            // Two hexadecimal digits make one byte.
            *byte = (digits.next()?? << 4) | digits.next()??;
        }
        Some(Self(bytes))
    }
}

/// Reverses the bytes of each of the first three groups of a UUID's bytes, which converts them
/// between the order of its text and the layout of EFI GUIDs, either way.
const fn swap_groups(bytes: [u8; 16]) -> [u8; 16] {
    let [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p] = bytes;
    [d, c, b, a, f, e, h, g, i, j, k, l, m, n, o, p]
}

impl fmt::Display for Uuid {
    /// Writes the UUID's canonical text.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, byte) in self.0.iter().enumerate() {
            if matches!(index, 4 | 6 | 8 | 10) {
                formatter.write_str("-")?;
            }
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Uuid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Uuid({self})")
    }
}

impl FromStr for Uuid {
    type Err = ParseUuidError;

    /// Parses the canonical text of a UUID, as [`Uuid::parse`] does.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text).ok_or(ParseUuidError(()))
    }
}

/// Why a string is not a UUID in its canonical text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseUuidError(());

impl fmt::Display for ParseUuidError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            "not a UUID in its canonical text: 32 lowercase hexadecimal digits in groups of 8, 4, \
             4, 4, and 12, joined by hyphens",
        )
    }
}

impl std::error::Error for ParseUuidError {}

#[cfg(test)]
mod tests;
