//! The CRC-32 that protects GPT headers and partition entry arrays.
//!
//! A CRC-32 is 32 bits, so the CRC, its polynomial, and each entry of its table are `u32`s. UEFI
//! specifies the CRC-32 of IEEE 802.3, which zlib also computes: the reflected polynomial
//! `0xEDB88320`, an all-ones initial value, and an inverted result.
//!
//! The module computes it a byte at a time, from a table of 256 entries, the CRC of each value of
//! a byte, which the compiler builds, so that GPT parsing depends on nothing outside the standard
//! library.

/// The reflected IEEE 802.3 polynomial.
const POLYNOMIAL: u32 = 0xEDB8_8320;

/// The CRC of every value of a byte, indexed by the byte: 256 entries of 32 bits.
const TABLE: [u32; 256] = table();

/// Builds [`TABLE`].
const fn table() -> [u32; 256] {
    let mut table = [0; 256];
    let mut byte = 0;
    while byte < 256 {
        let mut crc = byte as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 0 {
                crc >> 1
            } else {
                (crc >> 1) ^ POLYNOMIAL
            };
            bit += 1;
        }
        table[byte] = crc;
        byte += 1;
    }
    table
}

/// A CRC-32 over bytes that arrive in parts.
#[derive(Clone, Copy, Debug)]
pub(super) struct Crc32 {
    /// The register, before the final inversion.
    state: u32,
}

impl Crc32 {
    /// Starts a CRC over no bytes.
    pub(super) fn new() -> Self {
        Self { state: u32::MAX }
    }

    /// Adds `bytes` to the CRC.
    pub(super) fn update(mut self, bytes: &[u8]) -> Self {
        for byte in bytes {
            let index = usize::from((self.state as u8) ^ byte);
            self.state = (self.state >> 8) ^ TABLE[index];
        }
        self
    }

    /// Returns the CRC of every byte added.
    pub(super) fn finish(self) -> u32 {
        !self.state
    }
}

/// Returns the CRC-32 of `bytes`.
pub(super) fn crc32(bytes: &[u8]) -> u32 {
    Crc32::new().update(bytes).finish()
}

#[cfg(test)]
mod tests;
