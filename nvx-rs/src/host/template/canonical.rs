//! The canonical encoding of a template's compatibility key and of a capture class: the bytes
//! whose SHA-256 digest is their identity.
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
mod tests {
    use super::*;

    #[test]
    fn values_round_trip_in_their_one_encoding() {
        let mut writer = Writer::default();
        writer.u8(0xab);
        writer.u16(0x0102);
        writer.u32(0x0304_0506);
        writer.u64(0x0708_090a_0b0c_0d0e);
        writer.bool(true);
        writer.fixed(&[9, 8]);
        writer.text("héllo");
        writer.option(Some(&7u32), |writer, value| writer.u32(*value));
        writer.option(None::<&u32>, |writer, value| writer.u32(*value));
        writer.list(&[1u8, 2], |writer, item| writer.u8(*item));
        let bytes = writer.finish();
        assert_eq!(
            bytes,
            [
                0xab, 0x02, 0x01, 0x06, 0x05, 0x04, 0x03, 0x0e, 0x0d, 0x0c, 0x0b, 0x0a, 0x09, 0x08,
                0x07, 1, 9, 8, 6, 0, 0, 0, b'h', 0xc3, 0xa9, b'l', b'l', b'o', 1, 7, 0, 0, 0, 0, 2,
                0, 0, 0, 1, 2
            ]
        );
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.u8("a").unwrap(), 0xab);
        assert_eq!(reader.u16("b").unwrap(), 0x0102);
        assert_eq!(reader.u32("c").unwrap(), 0x0304_0506);
        assert_eq!(reader.u64("d").unwrap(), 0x0708_090a_0b0c_0d0e);
        assert!(reader.bool("e").unwrap());
        assert_eq!(reader.fixed::<2>("f").unwrap(), [9, 8]);
        assert_eq!(reader.text(6, "g").unwrap(), "héllo");
        assert_eq!(
            reader.option("h", |reader| reader.u32("h")).unwrap(),
            Some(7)
        );
        assert_eq!(reader.option("i", |reader| reader.u32("i")).unwrap(), None);
        assert_eq!(
            reader.list(2, "j", |reader| reader.u8("j")).unwrap(),
            [1, 2]
        );
        reader.finish().unwrap();
    }

    #[test]
    fn other_encodings_are_refused() {
        let refused = |bytes: &[u8], read: fn(&mut Reader<'_>) -> Result<(), CanonicalError>| {
            let mut reader = Reader::new(bytes);
            read(&mut reader)
                .and_then(|()| reader.finish())
                .unwrap_err()
                .to_string()
        };
        assert_eq!(
            refused(&[2], |reader| reader.bool("flag").map(drop)),
            "flag is 2, not a boolean"
        );
        assert_eq!(
            refused(&[2, 0], |reader| reader
                .option("value", |reader| reader.u8("value"))
                .map(drop)),
            "value has the presence tag 2, not 0 or 1"
        );
        assert_eq!(
            refused(&[3, 0, 0, 0, b'a', b'b', b'c'], |reader| reader
                .text(2, "name")
                .map(drop)),
            "name holds 3 bytes, more than 2"
        );
        assert_eq!(
            refused(&[2, 0, 0, 0, 0xff, 0xfe], |reader| reader
                .text(2, "name")
                .map(drop)),
            "name is not UTF-8"
        );
        assert_eq!(
            refused(&[2, 0, 0, 0, b'a'], |reader| reader
                .text(2, "name")
                .map(drop)),
            "name is truncated"
        );
        assert_eq!(
            refused(&[0xff, 0xff, 0, 0], |reader| {
                reader
                    .list(usize::MAX, "items", |reader| reader.u8("item"))
                    .map(drop)
            }),
            "items is truncated"
        );
        assert_eq!(
            refused(&[3, 0, 0, 0], |reader| {
                reader
                    .list(2, "items", |reader| reader.u8("item"))
                    .map(drop)
            }),
            "items holds 3 items, more than 2"
        );
        assert_eq!(
            refused(&[1, 0, 9], |reader| reader.u16("number").map(drop)),
            "a byte follows the value"
        );
        assert_eq!(
            refused(&[1, 0, 9, 9], |reader| reader.u16("number").map(drop)),
            "2 bytes follow the value"
        );
        assert_eq!(
            refused(&[1], |reader| reader.u32("number").map(drop)),
            "number is truncated"
        );
    }
}
