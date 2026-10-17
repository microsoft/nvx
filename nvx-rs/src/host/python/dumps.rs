//! `json.dumps(value, indent=2, sort_keys=True)`, with which `nvx.py` writes its JSON files.
//!
//! [`Value`] holds what the ports write: Python's `None`, `bool`, `int` of any size, `str`, and
//! lists and dicts of them, and [`dumps`] writes it as Python does: ASCII text with one item or
//! member per line, each line indented by two spaces per level and ended by `,` where another
//! follows, members sorted by name and written as `"name": value`, and empty containers as `[]`
//! and `{}`. Like `ensure_ascii`, it escapes every character outside the printable ASCII range
//! `' '..='~'`, so `DEL` and anything beyond ASCII become `\uXXXX`, with surrogate pairs for
//! characters beyond the Basic Multilingual Plane.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::builtins::Int;

/// A value that [`dumps`] writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Value {
    /// `None`, which JSON writes as `null`.
    Null,
    /// `True` or `False`.
    Bool(bool),
    /// An `int` of any size.
    Int(Int),
    /// A `str`.
    Str(String),
    /// A `list`.
    List(Vec<Value>),
    /// A `dict` with string keys, which `sort_keys` writes in code point order, as a
    /// `BTreeMap` iterates UTF-8 strings.
    Dict(BTreeMap<String, Value>),
}

impl From<&str> for Value {
    fn from(text: &str) -> Self {
        Self::Str(text.to_owned())
    }
}

impl From<String> for Value {
    fn from(text: String) -> Self {
        Self::Str(text)
    }
}

impl From<u64> for Value {
    fn from(value: u64) -> Self {
        Self::Int(Int::from(value))
    }
}

impl From<u32> for Value {
    fn from(value: u32) -> Self {
        Self::Int(Int::from(value))
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Self::Int(Int::from(value))
    }
}

impl From<Int> for Value {
    fn from(value: Int) -> Self {
        Self::Int(value)
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(value: Option<T>) -> Self {
        value.map_or(Self::Null, Into::into)
    }
}

/// Builds a [`Value::Dict`] from its members.
pub(crate) fn dict<const N: usize>(members: [(&str, Value); N]) -> Value {
    Value::Dict(
        members
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect(),
    )
}

/// Returns `json.dumps(value, indent=2, sort_keys=True)`, without a trailing newline.
pub(crate) fn dumps(value: &Value) -> String {
    let mut output = String::new();
    write_value(&mut output, value, 0);
    output
}

/// Appends `value` at indentation `level`, whose first line the caller has already indented.
fn write_value(output: &mut String, value: &Value, level: usize) {
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(true) => output.push_str("true"),
        Value::Bool(false) => output.push_str("false"),
        Value::Int(value) => {
            let _ = write!(output, "{value}");
        }
        Value::Str(text) => write_string(output, text),
        Value::List(items) if items.is_empty() => output.push_str("[]"),
        Value::List(items) => {
            output.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                newline(output, level + 1);
                write_value(output, item, level + 1);
            }
            newline(output, level);
            output.push(']');
        }
        Value::Dict(members) if members.is_empty() => output.push_str("{}"),
        Value::Dict(members) => {
            output.push('{');
            for (index, (name, member)) in members.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                newline(output, level + 1);
                write_string(output, name);
                output.push_str(": ");
                write_value(output, member, level + 1);
            }
            newline(output, level);
            output.push('}');
        }
    }
}

/// Starts a new line indented to `level`.
fn newline(output: &mut String, level: usize) {
    output.push('\n');
    for _ in 0..level {
        output.push_str("  ");
    }
}

/// Appends `text` as `json.dumps` quotes it with `ensure_ascii`.
fn write_string(output: &mut String, text: &str) {
    output.push('"');
    for character in text.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{8}' => output.push_str("\\b"),
            '\u{c}' => output.push_str("\\f"),
            ' '..='~' => output.push(character),
            _ => {
                let mut units = [0u16; 2];
                for unit in character.encode_utf16(&mut units) {
                    let _ = write!(output, "\\u{unit:04x}");
                }
            }
        }
    }
    output.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_written_as_json_dumps_writes_them() {
        let value = dict([
            ("zeta", Value::List(vec![Value::from(1u64), Value::Null])),
            ("alpha", Value::Bool(true)),
            ("empty", Value::List(Vec::new())),
            (
                "nested",
                dict([("b", Value::from("x")), ("a", Value::Bool(false))]),
            ),
            ("none", Value::Dict(BTreeMap::new())),
            (
                "big",
                Value::Int(Int::parse("-123456789012345678901234567890").unwrap()),
            ),
        ]);
        assert_eq!(
            dumps(&value),
            "{\n  \"alpha\": true,\n  \"big\": -123456789012345678901234567890,\n  \
             \"empty\": [],\n  \"nested\": {\n    \"a\": false,\n    \"b\": \"x\"\n  },\n  \
             \"none\": {},\n  \"zeta\": [\n    1,\n    null\n  ]\n}"
        );
    }

    #[test]
    fn strings_are_escaped_to_ascii() {
        assert_eq!(
            dumps(&Value::from(
                "a\"b\\c\n\r\t\u{8}\u{c}\u{0}\u{1f} ~\u{7f}é\u{20ac}\u{1f600}"
            )),
            "\"a\\\"b\\\\c\\n\\r\\t\\b\\f\\u0000\\u001f ~\\u007f\\u00e9\\u20ac\\ud83d\\ude00\""
        );
    }

    #[test]
    fn keys_sort_by_code_point() {
        let value = dict([("é", Value::Null), ("z", Value::Null), ("Z", Value::Null)]);
        assert_eq!(
            dumps(&value),
            "{\n  \"Z\": null,\n  \"z\": null,\n  \"\\u00e9\": null\n}"
        );
    }
}
