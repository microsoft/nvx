//! The VM generation ID, which distinguishes the VM processes that run one guest.

use std::fmt;

/// The VM generation ID: 16 random bytes that each VM process of a guest has, from cold boot or
/// from a restore of a snapshot, so that clones of one snapshot have different IDs.
///
/// On x86-64, the time ABI's `TimeAbi::generation_id` reads it, and a restore's fresh entropy,
/// a reseed's [`Seed`](super::reseed::Seed), starts with the new one. Its `Display` form is 32
/// lowercase hexadecimal digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct GenerationId([u8; GenerationId::LENGTH]);

impl GenerationId {
    /// The ID's length in bytes.
    pub const LENGTH: usize = 16;

    /// Returns the ID whose bytes are `bytes`.
    pub const fn from_bytes(bytes: [u8; Self::LENGTH]) -> Self {
        Self(bytes)
    }

    /// Returns the ID's bytes.
    pub const fn to_bytes(self) -> [u8; Self::LENGTH] {
        self.0
    }
}

impl fmt::Display for GenerationId {
    /// Writes the ID as 32 lowercase hexadecimal digits.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0
            .iter()
            .try_for_each(|byte| write!(formatter, "{byte:02x}"))
    }
}

impl fmt::Debug for GenerationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "GenerationId({self})")
    }
}

#[cfg(test)]
mod tests;
