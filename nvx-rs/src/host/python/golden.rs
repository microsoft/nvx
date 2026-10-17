//! Checks the Python semantics against `tests/fixtures/python.json`, which the retired
//! `scripts/nvx_rs_fixtures.py` recorded from Python, and the helpers of the other golden tests.

use std::fmt::Debug;

use serde_json::{Value, json};

use super::builtins::{Int, is_space, repr, split, strip};
use super::json::{Document, Loads, Node, NodeId, ReadError, read, syntax_error};
use super::path::{Flavor, PurePath, windows_normpath};
use super::text::{PyText, decode_utf8};
use super::unicode::{DECIMAL_STARTS, NON_PRINTABLE};
use super::uuid::parse_uuid;

/// Returns the fixture `name` of `tests/fixtures`.
pub(crate) fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{path}: {error}"))
}

/// Returns the cases of a fixture's section.
pub(crate) fn cases<'a>(fixture: &'a Value, section: &str) -> &'a [Value] {
    let cases = fixture[section]
        .as_array()
        .unwrap_or_else(|| panic!("the fixture has no section {section}"));
    assert!(!cases.is_empty(), "section {section} has no cases");
    cases
}

/// Returns a string of a case.
pub(crate) fn text<'a>(case: &'a Value, key: &str) -> &'a str {
    case[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} is not a string in {case}"))
}

/// Returns a list of strings of a case.
pub(crate) fn texts(case: &Value, key: &str) -> Vec<String> {
    case[key]
        .as_array()
        .unwrap_or_else(|| panic!("{key} is not a list in {case}"))
        .iter()
        .map(|value| value.as_str().expect("a string").to_owned())
        .collect()
}

/// Returns the flavor of a case.
pub(crate) fn flavor(case: &Value) -> Flavor {
    match text(case, "flavor") {
        "posix" => Flavor::Posix,
        "windows" => Flavor::Windows,
        other => panic!("unknown flavor {other}"),
    }
}

/// Returns the bytes of a hex string.
pub(crate) fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).expect("hex"))
        .collect()
}

/// Collects the cases on which the port and Python differ, to report them together.
pub(crate) struct Mismatches {
    /// The section being checked.
    section: String,
    /// The number of cases checked.
    checked: usize,
    /// A description of each mismatch.
    found: Vec<String>,
}

impl Mismatches {
    /// Starts checking a section.
    pub(crate) fn new(section: &str) -> Self {
        Self {
            section: section.to_owned(),
            checked: 0,
            found: Vec::new(),
        }
    }

    /// Records whether the port's `actual` value for `case` equals Python's `expected` value.
    pub(crate) fn check<T: PartialEq + Debug>(&mut self, case: &Value, expected: T, actual: T) {
        self.checked += 1;
        if expected != actual {
            self.found.push(format!(
                "{case}\n  python: {expected:?}\n  rust:   {actual:?}"
            ));
        }
    }

    /// Fails with the mismatches, if any.
    pub(crate) fn finish(self) {
        assert!(self.checked > 0, "{} checked no cases", self.section);
        assert!(
            self.found.is_empty(),
            "{} of {} {} cases differ from Python:\n{}",
            self.found.len(),
            self.checked,
            self.section,
            self.found
                .iter()
                .take(20)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}

/// Returns the ranges of code points from `start` that satisfy `predicate`, as the generator
/// lists them.
fn ranges(start: u32, predicate: impl Fn(char) -> bool) -> Vec<(u32, u32)> {
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    for code in start..=0x10_FFFF {
        // Python's predicates are false for surrogates, which no `char` holds.
        if char::from_u32(code).is_some_and(&predicate) {
            match ranges.last_mut() {
                Some((_, last)) if *last + 1 == code => *last = code,
                _ => ranges.push((code, code)),
            }
        }
    }
    ranges
}

/// Returns a fixture's list of inclusive ranges.
fn fixture_ranges(value: &Value) -> Vec<(u32, u32)> {
    serde_json::from_value(value.clone()).expect("ranges")
}

#[test]
fn unicode_tables_match_python() {
    let fixture = fixture("python.json");
    assert_eq!(ranges(0, is_space), fixture_ranges(&fixture["space"]));
    let starts: Vec<u32> = serde_json::from_value(fixture["decimal_starts"].clone()).unwrap();
    assert_eq!(DECIMAL_STARTS, starts.as_slice());
    assert_eq!(
        NON_PRINTABLE,
        fixture_ranges(&fixture["non_printable"]).as_slice()
    );
}

#[test]
fn strings_integers_and_uuids_match_python() {
    let fixture = fixture("python.json");
    let mut mismatches = Mismatches::new("strings");
    for case in cases(&fixture, "strings") {
        let value = text(case, "text");
        mismatches.check(case, text(case, "repr"), repr(value).as_str());
        mismatches.check(
            case,
            texts(case, "split"),
            split(value).map(str::to_owned).collect(),
        );
        mismatches.check(case, text(case, "strip"), strip(value));
        mismatches.check(
            case,
            case["uuid"].as_str().map(str::to_owned),
            parse_uuid(value),
        );
        let expected = match case.get("int") {
            Some(int) => Ok(int.as_str().unwrap().to_owned()),
            None => Err(text(case, "int_error").to_owned()),
        };
        let actual = Int::parse(value)
            .map(|int| int.to_string())
            .map_err(|error| error.message(value));
        mismatches.check(case, expected, actual);
    }
    mismatches.finish();
}

#[test]
fn pure_paths_match_pathlib() {
    let fixture = fixture("python.json");
    let mut mismatches = Mismatches::new("paths");
    for case in cases(&fixture, "paths") {
        let path = PurePath::new(flavor(case), text(case, "text"));
        let parents: Vec<String> = path
            .parents()
            .map(|parent| parent.as_str().to_owned())
            .collect();
        mismatches.check(
            case,
            (
                text(case, "str"),
                texts(case, "parts"),
                case["absolute"].as_bool().unwrap(),
                text(case, "name"),
                texts(case, "parents"),
            ),
            (
                path.as_str(),
                path.parts(),
                path.is_absolute(),
                path.name(),
                parents,
            ),
        );
    }
    mismatches.finish();

    let mut mismatches = Mismatches::new("joins");
    for case in cases(&fixture, "joins") {
        let path = PurePath::new(flavor(case), text(case, "left")).join(text(case, "right"));
        mismatches.check(case, text(case, "str"), path.as_str());
    }
    mismatches.finish();

    let mut mismatches = Mismatches::new("equality");
    for case in cases(&fixture, "equality") {
        let left = PurePath::new(Flavor::Windows, text(case, "left"));
        let right = PurePath::new(Flavor::Windows, text(case, "right"));
        mismatches.check(case, case["equal"].as_bool().unwrap(), left == right);
    }
    mismatches.finish();

    let mut mismatches = Mismatches::new("normpath");
    for case in cases(&fixture, "normpath") {
        mismatches.check(
            case,
            text(case, "normpath"),
            windows_normpath(text(case, "text")).as_str(),
        );
    }
    mismatches.finish();
}

/// Returns a string in the generator's tagged form.
fn tagged_string(string: &PyText) -> Value {
    match string.as_str() {
        Some(text) => json!(["str", text]),
        None => json!(["code_points", string.code_points().collect::<Vec<_>>()]),
    }
}

/// Returns a decoded value in the generator's tagged form, with the members of an object as a
/// Python `dict` holds them: in the order of their first keys, with the last values.
fn tagged(document: &Document, id: NodeId) -> Value {
    match document.node(id) {
        Node::Null => json!(["null"]),
        Node::Bool(value) => json!(["bool", value]),
        // `str(int(literal))`, which drops the sign of `-0`.
        Node::Integer(literal) => json!(["int", Int::parse(literal).unwrap().to_string()]),
        // Python's `NaN` is a positive quiet NaN.
        Node::Float(value) if value.is_nan() && value.is_sign_positive() => {
            json!(["float", "7ff8000000000000"])
        }
        Node::Float(value) => json!(["float", format!("{:016x}", value.to_bits())]),
        Node::String(string) => tagged_string(string),
        Node::Array(items) => json!([
            "list",
            items
                .iter()
                .map(|&item| tagged(document, item))
                .collect::<Vec<_>>()
        ]),
        Node::Object(members) => {
            let mut entries: Vec<(&PyText, NodeId)> = Vec::new();
            for (key, value) in members {
                match entries.iter().position(|&(name, _)| name == key) {
                    Some(position) => entries[position].1 = *value,
                    None => entries.push((key, *value)),
                }
            }
            json!([
                "dict",
                entries
                    .iter()
                    .map(|&(key, value)| json!([tagged_string(key), tagged(document, value)]))
                    .collect::<Vec<_>>()
            ])
        }
    }
}

#[test]
fn json_documents_decode_as_python_decodes_them() {
    let fixture = fixture("python.json");
    let mut mismatches = Mismatches::new("json");
    for case in cases(&fixture, "json") {
        let expected = if let Some(value) = case.get("value") {
            Ok(value.clone())
        } else if let Some(message) = case.get("decode_error") {
            Err(("decode", message.as_str().unwrap().to_owned()))
        } else {
            Err(("value", text(case, "value_error").to_owned()))
        };
        let source = text(case, "text");
        let actual = match read(source, &mut Loads) {
            Ok(document) => Ok(tagged(&document, document.root())),
            Err(ReadError::Syntax { message, offset }) => {
                Err(("decode", syntax_error(source, message, offset)))
            }
            Err(ReadError::Hook(error)) => Err(("value", error.message(source))),
        };
        mismatches.check(case, expected, actual);
    }
    mismatches.finish();

    let mut mismatches = Mismatches::new("utf8");
    for case in cases(&fixture, "utf8") {
        mismatches.check(
            case,
            case["error"].as_str().map(str::to_owned),
            decode_utf8(&hex(text(case, "bytes"))).err(),
        );
    }
    mismatches.finish();
}
