//! The canonical encoding of a snapshot key and of a capture class. The key's compatibility
//! prefix, and the complete class encoding, are the bytes whose SHA-256 digest is their identity.
//!
//! Every value has exactly one encoding, and the reader refuses any other:
//!
//! - An integer is its fixed number of little-endian bytes: one for a `u8`, two for a `u16`,
//!   four for a `u32`, and eight for a `u64`.
//! - A boolean is one byte, 0 or 1.
//! - A byte string of fixed length, such as a digest or an address, is its bytes.
//! - A byte string of variable length is its length, a `u32`, then its bytes. A text is the byte
//!   string of its UTF-8 encoding.
//! - An optional value is one byte, 0 without the value and 1 before it.
//! - A list is its count, a `u32`, then its items in order.
//!
//! The reader bounds each length and count by the most that the value may hold, and refuses bytes
//! left after the value.

use std::fmt;

/// Why bytes are not the canonical encoding of a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CanonicalError(String);

impl CanonicalError {
    /// Returns the error that `problem` describes.
    pub(super) fn new(problem: impl Into<String>) -> Self {
        Self(problem.into())
    }
}

impl fmt::Display for CanonicalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Writes values in their canonical encoding.
#[derive(Debug, Default)]
pub(super) struct Writer {
    /// The bytes written so far.
    bytes: Vec<u8>,
}

impl Writer {
    /// Returns the bytes written.
    pub(super) fn finish(self) -> Vec<u8> {
        self.bytes
    }

    /// Writes a `u8`.
    pub(super) fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    /// Writes a `u16`.
    pub(super) fn u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    /// Writes a `u32`.
    pub(super) fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    /// Writes a `u64`.
    pub(super) fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    /// Writes a boolean.
    pub(super) fn bool(&mut self, value: bool) {
        self.u8(u8::from(value));
    }

    /// Writes a byte string of fixed length.
    pub(super) fn fixed(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    /// Writes a count or a length, which callers bound well below 2^32.
    fn len(&mut self, len: usize) {
        self.u32(u32::try_from(len).expect("canonical lengths are bounded"));
    }

    /// Writes a byte string of variable length.
    pub(super) fn bytes(&mut self, bytes: &[u8]) {
        self.len(bytes.len());
        self.fixed(bytes);
    }

    /// Writes a text.
    pub(super) fn text(&mut self, text: &str) {
        self.bytes(text.as_bytes());
    }

    /// Writes an optional value, with `write` for the value.
    pub(super) fn option<T>(&mut self, value: Option<&T>, write: impl FnOnce(&mut Self, &T)) {
        match value {
            None => self.u8(0),
            Some(value) => {
                self.u8(1);
                write(self, value);
            }
        }
    }

    /// Writes a list, with `write` for each item.
    pub(super) fn list<T>(&mut self, items: &[T], mut write: impl FnMut(&mut Self, &T)) {
        self.len(items.len());
        for item in items {
            write(self, item);
        }
    }
}

/// Reads values from their canonical encoding, refusing any other.
#[derive(Debug)]
pub(super) struct Reader<'a> {
    /// The bytes that remain.
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    /// Returns a reader of `bytes`.
    pub(super) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    /// Fails unless every byte was read.
    pub(super) fn finish(self) -> Result<(), CanonicalError> {
        match self.bytes.len() {
            0 => Ok(()),
            1 => Err(CanonicalError::new("a byte follows the value")),
            count => Err(CanonicalError::new(format!(
                "{count} bytes follow the value"
            ))),
        }
    }

    /// Reads `count` bytes, which `what` names.
    fn take(&mut self, count: usize, what: &str) -> Result<&'a [u8], CanonicalError> {
        if count > self.bytes.len() {
            return Err(CanonicalError::new(format!("{what} is truncated")));
        }
        let (taken, rest) = self.bytes.split_at(count);
        self.bytes = rest;
        Ok(taken)
    }

    /// Reads `N` bytes, which `what` names.
    pub(super) fn fixed<const N: usize>(&mut self, what: &str) -> Result<[u8; N], CanonicalError> {
        Ok(self.take(N, what)?.try_into().expect("N bytes were taken"))
    }

    /// Reads a `u8`, which `what` names.
    pub(super) fn u8(&mut self, what: &str) -> Result<u8, CanonicalError> {
        Ok(self.fixed::<1>(what)?[0])
    }

    /// Reads a `u16`, which `what` names.
    pub(super) fn u16(&mut self, what: &str) -> Result<u16, CanonicalError> {
        Ok(u16::from_le_bytes(self.fixed(what)?))
    }

    /// Reads a `u32`, which `what` names.
    pub(super) fn u32(&mut self, what: &str) -> Result<u32, CanonicalError> {
        Ok(u32::from_le_bytes(self.fixed(what)?))
    }

    /// Reads a `u64`, which `what` names.
    pub(super) fn u64(&mut self, what: &str) -> Result<u64, CanonicalError> {
        Ok(u64::from_le_bytes(self.fixed(what)?))
    }

    /// Reads a boolean, which `what` names.
    pub(super) fn bool(&mut self, what: &str) -> Result<bool, CanonicalError> {
        match self.u8(what)? {
            0 => Ok(false),
            1 => Ok(true),
            byte => Err(CanonicalError::new(format!(
                "{what} is {byte}, not a boolean"
            ))),
        }
    }

    /// Reads a count of at most `max` of the `unit`, bytes or items, of the value that `what`
    /// names.
    fn len(&mut self, max: usize, unit: &str, what: &str) -> Result<usize, CanonicalError> {
        let len = self.u32(what)?;
        usize::try_from(len)
            .ok()
            .filter(|&len| len <= max)
            .ok_or_else(|| {
                CanonicalError::new(format!("{what} holds {len} {unit}, more than {max}"))
            })
    }

    /// Reads a byte string of at most `max` bytes, which `what` names.
    pub(super) fn bytes(&mut self, max: usize, what: &str) -> Result<&'a [u8], CanonicalError> {
        let len = self.len(max, "bytes", what)?;
        self.take(len, what)
    }

    /// Reads a text of at most `max` bytes, which `what` names.
    pub(super) fn text(&mut self, max: usize, what: &str) -> Result<&'a str, CanonicalError> {
        std::str::from_utf8(self.bytes(max, what)?)
            .map_err(|_| CanonicalError::new(format!("{what} is not UTF-8")))
    }

    /// Reads an optional value, which `what` names, with `read` for the value.
    pub(super) fn option<T>(
        &mut self,
        what: &str,
        read: impl FnOnce(&mut Self) -> Result<T, CanonicalError>,
    ) -> Result<Option<T>, CanonicalError> {
        match self.u8(what)? {
            0 => Ok(None),
            1 => read(self).map(Some),
            tag => Err(CanonicalError::new(format!(
                "{what} has the presence tag {tag}, not 0 or 1"
            ))),
        }
    }

    /// Reads a list of at most `max` items, which `what` names, with `read` for each item.
    pub(super) fn list<T>(
        &mut self,
        max: usize,
        what: &str,
        mut read: impl FnMut(&mut Self) -> Result<T, CanonicalError>,
    ) -> Result<Vec<T>, CanonicalError> {
        let count = self.len(max, "items", what)?;
        // Each item takes at least one byte, so a count beyond the bytes left is truncated.
        if count > self.bytes.len() {
            return Err(CanonicalError::new(format!("{what} is truncated")));
        }
        (0..count).map(|_| read(self)).collect()
    }
}

#[cfg(test)]
mod tests;
