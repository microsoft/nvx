//! The JSON of policy files and of sandbox distro manifests: RFC 8259 JSON, as `serde_json` reads
//! it, without a field repeated in any object, and nested at most [`MAX_JSON_DEPTH`] arrays and
//! objects deep.
//!
//! `serde_json` refuses `NaN`, `Infinity`, numbers beyond the range of a double, unpaired
//! surrogates, a byte-order mark, and trailing text. It reads integers beyond 64 bits, and `-0`,
//! as doubles, like numbers with a fraction or an exponent.
//!
//! Another crate of a build can enable `serde_json`'s `arbitrary_precision` feature for every
//! crate that uses it. With it, `serde_json` hands a visitor each number that is not a 64-bit
//! integer as a map of one field, which holds the number as written, and `-0` as the integer 0.
//! [`read_document`] reads those numbers as `serde_json` reads them without the feature, so that
//! what a document holds does not depend on the crates beside `nvx`. It tells such a map from an
//! object of the document with that field, which `serde_json`'s own `Value` takes for a number,
//! by the field's value: `serde_json` hands the text of a number as an owned string, and never a
//! string of a document so.

use std::cell::RefCell;
use std::fmt;
use std::sync::LazyLock;

use serde::de::{DeserializeSeed, Deserializer, Error as _, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

use super::MAX_JSON_DEPTH;

/// The field of the map in which `serde_json`, with its `arbitrary_precision` feature, hands a
/// visitor a number that is not a 64-bit integer.
const NUMBER_FIELD: &str = "$serde_json::private::Number";

/// Whether `serde_json` has its `arbitrary_precision` feature, with which it reads numbers beyond
/// the range of a double, which it otherwise refuses.
static ARBITRARY_PRECISION: LazyLock<bool> = LazyLock::new(|| "1e400".parse::<Number>().is_ok());

/// Why [`read_document`] refused a document.
#[derive(Debug)]
pub(crate) enum ReadError {
    /// The document is not JSON, as `serde_json` reports it, with its position.
    Syntax(serde_json::Error),
    /// An object repeats a field.
    Repeated {
        /// The path of the object, such as `allow[0].ports[1]`, or the name of the root, such as
        /// `egress policy`, for the root.
        object: String,
        /// The field.
        field: String,
    },
    /// Arrays and objects nest more than [`MAX_JSON_DEPTH`] deep.
    TooDeep {
        /// Line of the array or object that is too deep, from 1.
        line: usize,
        /// Column of the array or object that is too deep, from 1.
        column: usize,
    },
}

/// What [`Seed`] refused, which the error of `serde_json` cannot carry.
#[derive(Debug)]
enum Refusal {
    /// The object at `object` repeats `field`.
    Repeated {
        /// The path of the object.
        object: String,
        /// The field.
        field: String,
    },
    /// An array or object is too deep.
    TooDeep,
}

/// Reads `text` as the JSON of a policy file.
pub(super) fn read(text: &str) -> Result<Value, ReadError> {
    read_document(text, "egress policy")
}

/// Reads `text` as the JSON of a document whose root errors name `root`, as in
/// `egress policy has duplicate field "allow"`.
pub(crate) fn read_document(text: &str, root: &str) -> Result<Value, ReadError> {
    let refusal = RefCell::new(None);
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let seed = Seed {
        location: Location::Root(root),
        depth: 0,
        refusal: &refusal,
        arbitrary_precision: *ARBITRARY_PRECISION,
    };
    let value = seed
        .deserialize(&mut deserializer)
        .and_then(|value| deserializer.end().map(|()| value));
    value.map_err(|error| match refusal.into_inner() {
        Some(Refusal::Repeated { object, field }) => ReadError::Repeated { object, field },
        Some(Refusal::TooDeep) => ReadError::TooDeep {
            line: error.line(),
            column: error.column(),
        },
        None => ReadError::Syntax(error),
    })
}

/// Where a value lies in a document.
#[derive(Debug, Clone, Copy)]
enum Location<'a> {
    /// The root, with the name that messages give it, such as `egress policy`.
    Root(&'a str),
    /// The value of a field of an object.
    Field(&'a Location<'a>, &'a str),
    /// An element of an array.
    Element(&'a Location<'a>, usize),
}

impl Location<'_> {
    /// Returns the path of the value, as messages name fields: the name of the root for the root,
    /// and otherwise fields and elements as in `allow[0].to[1]`, with a field whose name is not an
    /// identifier quoted, as in `["a b"]`.
    fn path(&self) -> String {
        match self {
            Self::Root(name) => (*name).to_owned(),
            _ => self.to_string(),
        }
    }
}

impl fmt::Display for Location<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Root(_) => Ok(()),
            Self::Field(parent, name) => {
                write!(formatter, "{parent}")?;
                let identifier = name
                    .chars()
                    .next()
                    .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
                    && name
                        .chars()
                        .all(|character| character.is_ascii_alphanumeric() || character == '_');
                match (identifier, parent) {
                    (false, _) => write!(formatter, "[{name:?}]"),
                    (true, Self::Root(_)) => formatter.write_str(name),
                    (true, _) => write!(formatter, ".{name}"),
                }
            }
            Self::Element(parent, index) => write!(formatter, "{parent}[{index}]"),
        }
    }
}

/// Reads a value as `serde_json` reads a [`Value`], but refuses an object that repeats a field
/// and an array or object nested too deep.
#[derive(Debug, Clone, Copy)]
struct Seed<'a> {
    /// Where the value lies.
    location: Location<'a>,
    /// The number of arrays and objects that hold the value.
    depth: usize,
    /// What the seed refused, if anything.
    refusal: &'a RefCell<Option<Refusal>>,
    /// Whether `serde_json` hands numbers as with its `arbitrary_precision` feature.
    arbitrary_precision: bool,
}

impl Seed<'_> {
    /// Returns the depth of the values of an array or object that starts here, or fails if the
    /// array or object is too deep.
    fn enter<E: serde::de::Error>(&self) -> Result<usize, E> {
        let depth = self.depth + 1;
        if depth > MAX_JSON_DEPTH {
            *self.refusal.borrow_mut() = Some(Refusal::TooDeep);
            return Err(E::custom("too deep"));
        }
        Ok(depth)
    }
}

impl<'de> DeserializeSeed<'de> for Seed<'_> {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Seed<'_> {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Value, E> {
        // Only `-0` arrives here as 0, and only with `arbitrary_precision`.
        if self.arbitrary_precision && value == 0 {
            return Ok(Value::from(-0.0));
        }
        Ok(value.into())
    }

    fn visit_u64<E>(self, value: u64) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_f64<E>(self, value: f64) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_str<E>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut elements: A) -> Result<Value, A::Error> {
        let depth = self.enter()?;
        let mut array = Vec::new();
        loop {
            let seed = Seed {
                location: Location::Element(&self.location, array.len()),
                depth,
                ..self
            };
            match elements.next_element_seed(seed)? {
                Some(element) => array.push(element),
                None => return Ok(Value::Array(array)),
            }
        }
    }

    fn visit_map<A: MapAccess<'de>>(self, mut members: A) -> Result<Value, A::Error> {
        let mut object = Map::new();
        // With `arbitrary_precision`, a map whose first field is `NUMBER_FIELD` may be a number,
        // which its value tells, so a map is entered once its first field is read. Otherwise it
        // is entered before, where an error that it is too deep points.
        let pending = if self.arbitrary_precision {
            let name = members.next_key::<String>()?;
            if name.as_deref() == Some(NUMBER_FIELD) {
                let seed = Seed {
                    location: Location::Field(&self.location, NUMBER_FIELD),
                    depth: self.depth + 1,
                    ..self
                };
                match members.next_value_seed(FirstValue(seed))? {
                    Handed::Number(text) => {
                        return number(&text)
                            .ok_or_else(|| A::Error::custom("number out of range"));
                    }
                    Handed::Value(value) => {
                        object.insert(NUMBER_FIELD.to_owned(), value);
                        Some(members.next_key::<String>()?)
                    }
                }
            } else {
                Some(name)
            }
        } else {
            None
        };
        let depth = self.enter()?;
        let mut next = match pending {
            Some(name) => name,
            None => members.next_key::<String>()?,
        };
        while let Some(name) = next {
            if object.contains_key(&name) {
                *self.refusal.borrow_mut() = Some(Refusal::Repeated {
                    object: self.location.path(),
                    field: name,
                });
                return Err(A::Error::custom("repeated field"));
            }
            let seed = Seed {
                location: Location::Field(&self.location, &name),
                depth,
                ..self
            };
            let value = members.next_value_seed(seed)?;
            object.insert(name, value);
            next = members.next_key::<String>()?;
        }
        Ok(Value::Object(object))
    }
}

/// The value of the field [`NUMBER_FIELD`] that starts a map, with `arbitrary_precision`.
enum Handed {
    /// The text of a number that `serde_json` read, which it hands as an owned string.
    Number(String),
    /// A value of an object of the document, which `serde_json` hands as it hands every value of
    /// a document, and a string borrowed or copied, never owned.
    Value(Value),
}

/// Reads the value of the field [`NUMBER_FIELD`] that starts a map, with `arbitrary_precision`:
/// the text of a number, or a value as [`Seed`] reads it.
struct FirstValue<'a>(Seed<'a>);

impl<'de> DeserializeSeed<'de> for FirstValue<'_> {
    type Value = Handed;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Handed, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for FirstValue<'_> {
    type Value = Handed;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.expecting(formatter)
    }

    fn visit_string<E>(self, text: String) -> Result<Handed, E> {
        Ok(Handed::Number(text))
    }

    fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Handed, E> {
        self.0.visit_bool(value).map(Handed::Value)
    }

    fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Handed, E> {
        self.0.visit_i64(value).map(Handed::Value)
    }

    fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Handed, E> {
        self.0.visit_u64(value).map(Handed::Value)
    }

    fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Handed, E> {
        self.0.visit_f64(value).map(Handed::Value)
    }

    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Handed, E> {
        self.0.visit_str(value).map(Handed::Value)
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<Handed, E> {
        self.0.visit_unit().map(Handed::Value)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, elements: A) -> Result<Handed, A::Error> {
        self.0.visit_seq(elements).map(Handed::Value)
    }

    fn visit_map<A: MapAccess<'de>>(self, members: A) -> Result<Handed, A::Error> {
        self.0.visit_map(members).map(Handed::Value)
    }
}

/// Returns the number that `serde_json`, with its `arbitrary_precision` feature, handed as written,
/// as `serde_json` reads it without the feature: as a double, which must be finite.
fn number(text: &str) -> Option<Value> {
    let double = text.parse::<f64>().ok()?;
    Number::from_f64(double).map(Value::Number)
}

#[cfg(test)]
mod tests;
