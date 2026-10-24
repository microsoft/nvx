//! Helpers that the unit tests of several modules share: the conformance vectors of
//! `tests/vectors/`, and the reader of the line format that `tests/vectors/README.md` describes.

use std::fmt::Debug;

/// The conformance vectors of version 1 of the control protocol.
pub(crate) const NVXC_V1: &str = include_str!("../tests/vectors/nvxc-v1.txt");
/// The conformance vectors of version 1 of the activation record, with its configuration
/// identities, and of what Ready reports about an activation.
pub(crate) const ACTIVATION_V1: &str = include_str!("../tests/vectors/activation-v1.txt");
/// The conformance vectors of version 1 of the listener announcement.
#[cfg(all(feature = "guest", target_os = "linux"))]
pub(crate) const NVXL_V1: &str = include_str!("../tests/vectors/nvxl-v1.txt");
/// OpenVMM's vectors of the NVXS session, unchanged: one record of each type, as `NAME|BYTES`.
pub(crate) const OPENVMM_VECTORS: &str =
    include_str!("../tests/vectors/openvmm/control_session_protocol_vectors.txt");
/// OpenVMM's cases of the NVXS session, unchanged: the boundaries and refusals of its records.
pub(crate) const OPENVMM_CASES: &str =
    include_str!("../tests/vectors/openvmm/control_session_protocol_cases.txt");

/// One vector of a vector file, the line `KIND|NAME|BYTES|VALUE|...`.
#[derive(Debug)]
pub(crate) struct Vector {
    /// What the bytes encode.
    pub(crate) kind: &'static str,
    /// Name of the vector, unique among the vectors of its kind.
    pub(crate) name: &'static str,
    /// The encoding.
    pub(crate) bytes: Vec<u8>,
    /// The values that the bytes encode, each in its written form.
    values: Vec<&'static str>,
}

impl Vector {
    /// Returns a reader of the vector's values, which checks, once dropped, that every value was
    /// read.
    pub(crate) fn values(&self) -> Values<'_> {
        Values {
            vector: self,
            fields: self.values.iter(),
        }
    }
}

/// Returns the vectors of `file`, the contents of a vector file, in order.
///
/// It panics on a line that does not have the line format, and on a vector that repeats the kind
/// and name of another.
pub(crate) fn vectors(file: &'static str) -> Vec<Vector> {
    let mut vectors: Vec<Vector> = Vec::new();
    for line in file.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split('|');
        let (Some(kind), Some(name), Some(bytes)) = (fields.next(), fields.next(), fields.next())
        else {
            panic!("the vector {line:?} lacks a kind, a name, or bytes");
        };
        assert!(
            is_word(kind) && is_word(name),
            "the vector {line:?} has a malformed kind or name"
        );
        assert!(
            !vectors
                .iter()
                .any(|vector| (vector.kind, vector.name) == (kind, name)),
            "{kind} {name} repeats"
        );
        let values: Vec<_> = fields.collect();
        assert!(
            !kind.starts_with("INVALID_") || values.is_empty(),
            "{kind} {name} has values"
        );
        vectors.push(Vector {
            kind,
            name,
            bytes: hex(bytes),
            values,
        });
    }
    vectors
}

/// Returns the vectors of `kind` in `file`, which must be those named `names`, in order.
pub(crate) fn of_kind(file: &'static str, kind: &str, names: &[&str]) -> Vec<Vector> {
    let vectors: Vec<_> = vectors(file)
        .into_iter()
        .filter(|vector| vector.kind == kind)
        .collect();
    let found: Vec<_> = vectors.iter().map(|vector| vector.name).collect();
    assert_eq!(found, names, "the {kind} vectors");
    vectors
}

/// Returns the vector of `kind` named `name` in `file`.
pub(crate) fn vector(file: &'static str, kind: &str, name: &str) -> Vector {
    vectors(file)
        .into_iter()
        .find(|vector| vector.kind == kind && vector.name == name)
        .unwrap_or_else(|| panic!("no {kind} vector is named {name}"))
}

/// Decodes `text`, two lowercase hexadecimal digits per byte.
pub(crate) fn hex(text: &str) -> Vec<u8> {
    assert!(
        text.len().is_multiple_of(2) && text.bytes().all(|digit| is_digit(digit, 16)),
        "{text:?} is not lowercase hexadecimal"
    );
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}

/// Returns whether `digit` is a digit of `radix`, 10 or 16, in lowercase.
fn is_digit(digit: u8, radix: u32) -> bool {
    digit.is_ascii_digit() || (radix == 16 && matches!(digit, b'a'..=b'f'))
}

/// Returns whether `text` is a word in capitals: a capital letter, then capital letters, digits,
/// and underscores.
fn is_word(text: &str) -> bool {
    text.bytes()
        .next()
        .is_some_and(|first| first.is_ascii_uppercase())
        && text
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

/// Reader of the values of a vector, in order.
pub(crate) struct Values<'a> {
    /// The vector, which the reader's messages name.
    vector: &'a Vector,
    /// The values that remain to be read.
    fields: std::slice::Iter<'a, &'static str>,
}

impl Values<'_> {
    /// Returns the written form of the next value.
    fn field(&mut self) -> &'static str {
        let Vector { kind, name, .. } = self.vector;
        self.fields
            .next()
            .copied()
            .unwrap_or_else(|| panic!("{kind} {name} lacks a value"))
    }

    /// Reads a number: decimal, or hexadecimal after `0x`, and negative after `-`.
    pub(crate) fn number<T: TryFrom<i128>>(&mut self) -> T
    where
        T::Error: Debug,
    {
        let field = self.field();
        let (negative, magnitude) = match field.strip_prefix('-') {
            Some(magnitude) => (true, magnitude),
            None => (false, field),
        };
        let (radix, digits) = match magnitude.strip_prefix("0x") {
            Some(digits) => (16, digits),
            None => (10, magnitude),
        };
        assert!(
            !digits.is_empty() && digits.bytes().all(|digit| is_digit(digit, radix)),
            "{field:?} is not a number"
        );
        let value = i128::from_str_radix(digits, radix).unwrap();
        T::try_from(if negative { -value } else { value })
            .unwrap_or_else(|error| panic!("{field} is out of range: {error:?}"))
    }

    /// Reads a byte string.
    pub(crate) fn bytes(&mut self) -> Vec<u8> {
        hex(self.field())
    }

    /// Reads a text, and returns the bytes that it stands for.
    pub(crate) fn text(&mut self) -> Vec<u8> {
        let field = self.field();
        let mut bytes = Vec::with_capacity(field.len());
        let mut rest = field.as_bytes();
        while let Some((&byte, tail)) = rest.split_first() {
            if byte == b'%' {
                let escape = tail
                    .get(..2)
                    .filter(|digits| digits.iter().all(u8::is_ascii_hexdigit))
                    .unwrap_or_else(|| panic!("{field:?} has a malformed escape"));
                bytes.push(u8::from_str_radix(std::str::from_utf8(escape).unwrap(), 16).unwrap());
                rest = &tail[2..];
            } else {
                assert!(
                    (b' '..=b'~').contains(&byte),
                    "{field:?} holds a byte that it must escape"
                );
                bytes.push(byte);
                rest = tail;
            }
        }
        bytes
    }

    /// Reads a word in capitals.
    pub(crate) fn word(&mut self) -> &'static str {
        let field = self.field();
        assert!(is_word(field), "{field:?} is not a word in capitals");
        field
    }

    /// Reads a value with `read` unless `-` stands for its absence.
    pub(crate) fn optional<T>(&mut self, read: impl FnOnce(&mut Self) -> T) -> Option<T> {
        if self.fields.as_slice().first() == Some(&"-") {
            self.fields.next();
            None
        } else {
            Some(read(self))
        }
    }

    /// Reads a list: its number of items, then each item with `read`.
    pub(crate) fn list<T>(&mut self, mut read: impl FnMut(&mut Self) -> T) -> Vec<T> {
        let count: usize = self.number();
        (0..count).map(|_| read(self)).collect()
    }
}

impl Drop for Values<'_> {
    fn drop(&mut self) {
        let Vector { kind, name, .. } = self.vector;
        if !std::thread::panicking() {
            assert!(
                self.fields.as_slice().is_empty(),
                "{kind} {name} has more values than its kind"
            );
        }
    }
}

#[test]
fn the_reader_follows_the_line_format() {
    let file = "# A comment.\n\nKIND|NAME|00ff|-12|0x1f|cafe||a%7cb%25 c|WORD|-|2|x|y\n\
                INVALID_KIND|NAME|01\r\n";
    let vectors = vectors(file);
    assert_eq!(vectors.len(), 2);
    let vector = &vectors[0];
    assert_eq!(
        (vector.kind, vector.name, &vector.bytes[..]),
        ("KIND", "NAME", &[0, 0xff][..])
    );
    let mut values = vector.values();
    assert_eq!(values.number::<i32>(), -12);
    assert_eq!(values.number::<u8>(), 0x1f);
    assert_eq!(values.bytes(), [0xca, 0xfe]);
    assert!(values.bytes().is_empty());
    assert_eq!(values.text(), b"a|b% c");
    assert_eq!(values.word(), "WORD");
    assert_eq!(values.optional(Values::text), None);
    assert_eq!(values.list(Values::text), [b"x", b"y"]);
    drop(values);
    assert_eq!(vectors[1].bytes, [1]);
    assert!(vectors[1].values.is_empty());
}

#[test]
#[should_panic(expected = "KIND NAME repeats")]
fn the_reader_refuses_a_repeated_vector() {
    vectors("KIND|NAME|00\nKIND|NAME|01\n");
}

#[test]
#[should_panic(expected = "is not lowercase hexadecimal")]
fn the_reader_refuses_uppercase_bytes() {
    vectors("KIND|NAME|0A\n");
}

#[test]
#[should_panic(expected = "KIND NAME has more values than its kind")]
fn the_reader_refuses_values_left_unread() {
    let vector = &vectors("KIND|NAME|00|1|2\n")[0];
    let mut values = vector.values();
    values.number::<u8>();
}
