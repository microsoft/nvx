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

/// The bytes at which the second through fifth groups of a UUID begin, whose groups of 4, 2, 2,
/// 2, and 6 bytes spell the 8, 4, 4, 4, and 12 digits of its canonical text.
const GROUP_STARTS: [usize; 4] = [4, 6, 8, 10];

/// The offsets of the hyphens in the canonical text of a UUID, each of which precedes a group.
const HYPHENS: [usize; GROUP_STARTS.len()] = hyphens();

/// The length of the canonical text of a UUID: two digits a byte, and its hyphens.
const TEXT_LEN: usize = 2 * 16 + HYPHENS.len();

/// Returns the [`HYPHENS`]: each follows the two digits of every byte before its group, and the
/// hyphens before it.
const fn hyphens() -> [usize; GROUP_STARTS.len()] {
    let mut hyphens = [0; GROUP_STARTS.len()];
    let mut group = 0;
    while group < GROUP_STARTS.len() {
        hyphens[group] = 2 * GROUP_STARTS[group] + group;
        group += 1;
    }
    hyphens
}

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

    /// Returns the version-4 UUID of `random`, 16 random bytes, as RFC 9562 §5.4 defines it: the
    /// bytes with their version and variant bits set, which leaves 122 of them random.
    #[cfg(any(feature = "cli", all(feature = "guest", target_os = "linux")))]
    pub(crate) const fn v4(random: [u8; 16]) -> Self {
        /// The byte whose high four bits hold the version, as RFC 9562 §4.2 places it.
        const VERSION_BYTE: usize = 6;
        /// The version bits of `VERSION_BYTE`: its high four.
        const VERSION_MASK: u8 = 0xf0;
        /// The version bits of a version-4 UUID: 4 in the high four bits.
        const VERSION_4: u8 = 0x40;
        /// The byte whose high bits hold the variant, as RFC 9562 §4.1 places it.
        const VARIANT_BYTE: usize = 8;
        /// The variant bits of `VARIANT_BYTE`: its high two.
        const VARIANT_MASK: u8 = 0xc0;
        /// The variant bits of an RFC 9562 UUID: `0b10` in the high two bits.
        const VARIANT_RFC_9562: u8 = 0x80;
        let mut bytes = random;
        bytes[VERSION_BYTE] = (bytes[VERSION_BYTE] & !VERSION_MASK) | VERSION_4;
        bytes[VARIANT_BYTE] = (bytes[VARIANT_BYTE] & !VARIANT_MASK) | VARIANT_RFC_9562;
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
            if GROUP_STARTS.contains(&index) {
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
