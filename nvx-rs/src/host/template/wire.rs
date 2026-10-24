//! A strict reader of the protobuf wire format, in which OpenVMM's `mesh` encodes the manifest of
//! a capture.
//!
//! A message is a sequence of fields, each a key, which holds the field's number and wire type,
//! and a value of that type: a variable-length integer (varint), eight or four little-endian bytes,
//! or a length and that many bytes, which hold a string, bytes, a nested message, or packed
//! integers. `mesh` omits a field whose value is zero or empty, except an `Option` that is
//! `Some`, writes a repeated field once per element, and packs a list of integers into one field.
//!
//! The reader takes only what such a writer produces, within the bounds of its input: a group or
//! an unknown wire type, a varint longer than ten bytes or beyond 64 bits, a field number outside
//! 1 to 2^29 - 1, and a value that runs past its message are refused. [`Message`] refuses the
//! fields that a message type does not know, and a field that is not repeated but appears twice,
//! which a writer never produces. The reader never recurses on its own: each message type decodes
//! its nested messages, whose depth the manifest's types bound.

use std::fmt;

/// The largest field number of the wire format: 2^29 - 1.
const MAX_FIELD_NUMBER: u64 = (1 << 29) - 1;

/// The longest varint, in bytes: ten, for 64 bits.
const MAX_VARINT_LEN: usize = 10;

/// Why bytes are not a message, or a field is not of its type.
///
/// Its `Display` text is a predicate, whose subject, such as "the manifest", the caller supplies:
/// "is truncated", or "holds the unknown field 9".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum WireError {
    /// A key, a varint, or a value runs past the end of its message.
    Truncated,
    /// A varint is longer than ten bytes, or has bits beyond 64.
    Varint,
    /// A key names a group, or a wire type that does not exist.
    WireType(u8),
    /// A key names field 0, or a field number beyond 2^29 - 1.
    FieldNumber(u64),
    /// The message type does not know the field.
    Unknown(u32),
    /// A field that is not repeated appears twice.
    Repeated(u32),
    /// A field has another wire type than its type.
    Type {
        /// The field.
        number: u32,
        /// The type that the message type gives it.
        expected: &'static str,
    },
    /// A field's value is not of its type, for the reason that this gives.
    Value {
        /// The field.
        number: u32,
        /// Why the value is invalid, as a predicate of the value.
        problem: String,
    },
}

impl fmt::Display for WireError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => formatter.write_str("is truncated"),
            Self::Varint => formatter.write_str("holds an integer longer than 64 bits"),
            Self::WireType(wire_type) => {
                write!(formatter, "holds a field of wire type {wire_type}")
            }
            Self::FieldNumber(number) => write!(formatter, "holds a field numbered {number}"),
            Self::Unknown(number) => write!(formatter, "holds the unknown field {number}"),
            Self::Repeated(number) => write!(formatter, "holds field {number} more than once"),
            Self::Type { number, expected } => {
                write!(formatter, "holds field {number}, which is not {expected}")
            }
            Self::Value { number, problem } => {
                write!(formatter, "holds field {number}, which {problem}")
            }
        }
    }
}

/// The value of a field, as its wire type holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Value<'a> {
    /// Wire type 0: a varint.
    Varint(u64),
    /// Wire type 1: eight little-endian bytes.
    Fixed64(u64),
    /// Wire type 2: a length and that many bytes.
    Bytes(&'a [u8]),
    /// Wire type 5: four little-endian bytes.
    Fixed32(u32),
}

/// A field of a message: its number and its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Field<'a> {
    /// The field's number, from 1 to 2^29 - 1.
    pub(super) number: u32,
    /// The field's value.
    pub(super) value: Value<'a>,
}

impl<'a> Field<'a> {
    /// Returns the error of a field that is not of the wire type of `expected`, as messages name
    /// the type.
    fn mistyped(&self, expected: &'static str) -> WireError {
        WireError::Type {
            number: self.number,
            expected,
        }
    }

    /// Returns the error of a field whose value is invalid, for the reason `problem`, a predicate
    /// of the value.
    pub(super) fn invalid(&self, problem: impl Into<String>) -> WireError {
        WireError::Value {
            number: self.number,
            problem: problem.into(),
        }
    }

    /// Returns the field's varint.
    pub(super) fn uint64(&self) -> Result<u64, WireError> {
        match self.value {
            Value::Varint(value) => Ok(value),
            _ => Err(self.mistyped("an integer")),
        }
    }

    /// Returns the field's varint, which must fit 32 bits.
    pub(super) fn uint32(&self) -> Result<u32, WireError> {
        u32::try_from(self.uint64()?).map_err(|_| self.invalid("does not fit 32 bits"))
    }

    /// Returns the field's varint as a signed 64-bit integer in two's complement, as `mesh`
    /// encodes a timestamp's seconds.
    pub(super) fn int64(&self) -> Result<i64, WireError> {
        Ok(self.uint64()? as i64)
    }

    /// Returns the field's varint as a signed 32-bit integer in two's complement, which a
    /// negative one extends to 64 bits, as `mesh` encodes a timestamp's nanoseconds.
    pub(super) fn int32(&self) -> Result<i32, WireError> {
        i32::try_from(self.int64()?).map_err(|_| self.invalid("does not fit 32 bits"))
    }

    /// Returns the field's varint as a boolean: 0 or 1.
    pub(super) fn boolean(&self) -> Result<bool, WireError> {
        match self.uint64()? {
            0 => Ok(false),
            1 => Ok(true),
            value => Err(self.invalid(format!("is {value}, not a boolean"))),
        }
    }

    /// Returns the field's bytes.
    pub(super) fn bytes(&self) -> Result<&'a [u8], WireError> {
        match self.value {
            Value::Bytes(bytes) => Ok(bytes),
            _ => Err(self.mistyped("bytes")),
        }
    }

    /// Returns the field's bytes as UTF-8 text.
    pub(super) fn text(&self) -> Result<&'a str, WireError> {
        std::str::from_utf8(self.bytes()?).map_err(|_| self.invalid("is not UTF-8"))
    }

    /// Returns the field's packed varints, each of which must fit 32 bits.
    pub(super) fn packed_uint32(&self) -> Result<Vec<u32>, WireError> {
        let mut bytes = self.bytes()?;
        let mut values = Vec::new();
        while !bytes.is_empty() {
            let value = read_varint(&mut bytes)?;
            values.push(u32::try_from(value).map_err(|_| self.invalid("does not fit 32 bits"))?);
        }
        Ok(values)
    }
}

/// Reads a varint from the front of `data`, and advances `data` past it.
fn read_varint(data: &mut &[u8]) -> Result<u64, WireError> {
    let mut value = 0u64;
    for (index, &byte) in data.iter().enumerate().take(MAX_VARINT_LEN) {
        let bits = u64::from(byte & 0x7f);
        // The tenth byte holds the 64th bit alone.
        if index == MAX_VARINT_LEN - 1 && bits > 1 {
            return Err(WireError::Varint);
        }
        value |= bits << (7 * index);
        if byte & 0x80 == 0 {
            *data = &data[index + 1..];
            return Ok(value);
        }
    }
    if data.len() < MAX_VARINT_LEN {
        Err(WireError::Truncated)
    } else {
        Err(WireError::Varint)
    }
}

/// Splits `count` bytes from the front of `data`, and advances `data` past them.
fn take<'a>(data: &mut &'a [u8], count: u64) -> Result<&'a [u8], WireError> {
    let count = usize::try_from(count).map_err(|_| WireError::Truncated)?;
    if count > data.len() {
        return Err(WireError::Truncated);
    }
    let (taken, rest) = data.split_at(count);
    *data = rest;
    Ok(taken)
}

/// Reads one field from the front of `data`, and advances `data` past it.
fn read_field<'a>(data: &mut &'a [u8]) -> Result<Field<'a>, WireError> {
    let key = read_varint(data)?;
    let number = key >> 3;
    if number == 0 || number > MAX_FIELD_NUMBER {
        return Err(WireError::FieldNumber(number));
    }
    let number = number as u32;
    let value = match key & 7 {
        0 => Value::Varint(read_varint(data)?),
        1 => Value::Fixed64(u64::from_le_bytes(
            take(data, 8)?.try_into().expect("eight bytes were taken"),
        )),
        2 => {
            let len = read_varint(data)?;
            Value::Bytes(take(data, len)?)
        }
        5 => Value::Fixed32(u32::from_le_bytes(
            take(data, 4)?.try_into().expect("four bytes were taken"),
        )),
        wire_type => return Err(WireError::WireType(wire_type as u8)),
    };
    Ok(Field { number, value })
}

/// The fields of a message, in their order, which refuses an unknown field and a field that is
/// not repeated but appears twice, as its caller declares them.
pub(super) struct Message<'a> {
    /// The fields that remain to be read.
    data: &'a [u8],
    /// The numbers of the fields that the message type knows, each below 64.
    known: u64,
    /// The numbers of the known fields that are repeated.
    repeated: u64,
    /// The numbers of the fields read so far.
    seen: u64,
}

impl<'a> Message<'a> {
    /// Returns the fields of the message `data`, whose type knows the fields `known` and the
    /// fields `repeated`, which may appear more than once. Every number must be below 64.
    pub(super) fn new(data: &'a [u8], known: &[u32], repeated: &[u32]) -> Self {
        let mask = |numbers: &[u32]| {
            numbers.iter().fold(0u64, |mask, &number| {
                assert!(number < 64, "field {number} is beyond the reader's mask");
                mask | 1 << number
            })
        };
        let repeated = mask(repeated);
        Self {
            data,
            known: mask(known) | repeated,
            repeated,
            seen: 0,
        }
    }

    /// Returns the next field, `None` at the end of the message, or why the message is invalid.
    pub(super) fn next_field(&mut self) -> Result<Option<Field<'a>>, WireError> {
        if self.data.is_empty() {
            return Ok(None);
        }
        let field = read_field(&mut self.data)?;
        let bit = 1u64.checked_shl(field.number).unwrap_or(0);
        if bit == 0 || self.known & bit == 0 {
            return Err(WireError::Unknown(field.number));
        }
        if self.repeated & bit == 0 && self.seen & bit != 0 {
            return Err(WireError::Repeated(field.number));
        }
        self.seen |= bit;
        Ok(Some(field))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns every field of `data`, or the first error.
    fn fields<'a>(
        data: &'a [u8],
        known: &[u32],
        repeated: &[u32],
    ) -> Result<Vec<Field<'a>>, WireError> {
        let mut message = Message::new(data, known, repeated);
        let mut fields = Vec::new();
        while let Some(field) = message.next_field()? {
            fields.push(field);
        }
        Ok(fields)
    }

    #[test]
    fn each_wire_type_reads_its_value() {
        let data = [
            0x08, 0x96, 0x01, // 1: varint 150
            0x11, 1, 2, 3, 4, 5, 6, 7, 8, // 2: fixed64
            0x1a, 0x03, b'a', b'b', b'c', // 3: bytes "abc"
            0x25, 1, 0, 0, 0x80, // 4: fixed32
        ];
        let read = fields(&data, &[1, 2, 3, 4], &[]).unwrap();
        assert_eq!(
            read,
            [
                Field {
                    number: 1,
                    value: Value::Varint(150)
                },
                Field {
                    number: 2,
                    value: Value::Fixed64(0x0807_0605_0403_0201)
                },
                Field {
                    number: 3,
                    value: Value::Bytes(b"abc")
                },
                Field {
                    number: 4,
                    value: Value::Fixed32(0x8000_0001)
                },
            ]
        );
        assert_eq!(read[0].uint32().unwrap(), 150);
        assert_eq!(read[2].text().unwrap(), "abc");
        assert_eq!(
            read[0].bytes().unwrap_err(),
            WireError::Type {
                number: 1,
                expected: "bytes"
            }
        );
        assert_eq!(
            read[2].uint64().unwrap_err(),
            WireError::Type {
                number: 3,
                expected: "an integer"
            }
        );
    }

    #[test]
    fn varints_hold_64_bits_in_at_most_ten_bytes() {
        let max = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01];
        assert_eq!(read_varint(&mut &max[..]).unwrap(), u64::MAX);
        // A tenth byte with more than the 64th bit, and an eleventh byte, overflow.
        let mut over = max;
        over[9] = 0x02;
        assert_eq!(read_varint(&mut &over[..]), Err(WireError::Varint));
        let long = [0x80; 11];
        assert_eq!(read_varint(&mut &long[..]), Err(WireError::Varint));
        // A varint whose last byte is missing is truncated.
        assert_eq!(
            read_varint(&mut &[0x80, 0x80][..]),
            Err(WireError::Truncated)
        );
        assert_eq!(read_varint(&mut &[][..]), Err(WireError::Truncated));
        // A longer encoding than needed still reads.
        assert_eq!(read_varint(&mut &[0x81, 0x00][..]).unwrap(), 1);
    }

    #[test]
    fn signed_integers_are_twos_complement() {
        let minus_one = [
            0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01,
        ];
        let read = fields(&minus_one, &[1], &[]).unwrap();
        assert_eq!(read[0].int64().unwrap(), -1);
        assert_eq!(read[0].int32().unwrap(), -1);
        assert!(read[0].uint32().is_err());
        let large = [0x08, 0x80, 0x80, 0x80, 0x80, 0x10];
        assert!(fields(&large, &[1], &[]).unwrap()[0].int32().is_err());
    }

    #[test]
    fn booleans_and_packed_lists_are_strict() {
        let read = fields(&[0x08, 0x01, 0x10, 0x02], &[1, 2], &[]).unwrap();
        assert!(read[0].boolean().unwrap());
        assert_eq!(
            read[1].boolean().unwrap_err().to_string(),
            "holds field 2, which is 2, not a boolean"
        );
        let packed = [0x0a, 0x04, 0x01, 0x80, 0x01, 0x00];
        assert_eq!(
            fields(&packed, &[1], &[]).unwrap()[0]
                .packed_uint32()
                .unwrap(),
            [1, 128, 0]
        );
        let truncated = [0x0a, 0x02, 0x01, 0x80];
        assert_eq!(
            fields(&truncated, &[1], &[]).unwrap()[0].packed_uint32(),
            Err(WireError::Truncated)
        );
        let wide = [0x0a, 0x05, 0x80, 0x80, 0x80, 0x80, 0x10];
        assert!(
            fields(&wide, &[1], &[]).unwrap()[0]
                .packed_uint32()
                .is_err()
        );
    }

    #[test]
    fn malformed_messages_are_refused() {
        // A length past the end, a fixed value cut short, and a key without its value.
        assert_eq!(
            fields(&[0x0a, 0x05, b'a'], &[1], &[]),
            Err(WireError::Truncated)
        );
        assert_eq!(
            fields(&[0x09, 1, 2, 3], &[1], &[]),
            Err(WireError::Truncated)
        );
        assert_eq!(fields(&[0x08], &[1], &[]), Err(WireError::Truncated));
        // Groups and the wire types that do not exist.
        for (key, wire_type) in [(0x0b, 3), (0x0c, 4), (0x0e, 6), (0x0f, 7)] {
            assert_eq!(
                fields(&[key], &[1], &[]),
                Err(WireError::WireType(wire_type))
            );
        }
        // Field 0 and a number beyond 2^29 - 1.
        assert_eq!(
            fields(&[0x00, 0x00], &[1], &[]),
            Err(WireError::FieldNumber(0))
        );
        let beyond = [0x80, 0x80, 0x80, 0x80, 0x10, 0x00];
        assert_eq!(
            fields(&beyond, &[1], &[]),
            Err(WireError::FieldNumber(1 << 29))
        );
    }

    #[test]
    fn messages_refuse_unknown_and_repeated_fields() {
        assert_eq!(fields(&[0x10, 0x01], &[1], &[]), Err(WireError::Unknown(2)));
        // A field number beyond any that a message type knows is unknown too.
        assert_eq!(
            fields(&[0xc0, 0x0c, 0x01], &[1], &[]),
            Err(WireError::Unknown(200))
        );
        assert_eq!(
            fields(&[0x08, 0x01, 0x08, 0x02], &[1], &[]),
            Err(WireError::Repeated(1))
        );
        assert_eq!(
            fields(&[0x0a, 0x00, 0x0a, 0x00], &[], &[1]).unwrap().len(),
            2
        );
        assert_eq!(fields(&[], &[1], &[]).unwrap(), []);
    }
}
