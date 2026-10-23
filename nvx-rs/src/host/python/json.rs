//! A JSON reader that accepts exactly what Python's `json.loads` accepts, and refuses the rest as
//! Python refuses it.
//!
//! Python's decoder differs from strict JSON parsers such as `serde_json`:
//!
//! - it reads the constants `NaN`, `Infinity`, and `-Infinity` as floats, and numbers beyond the
//!   range of a float as infinities;
//! - it reads integers of any length, which `parse_int` receives as written, although `int`, its
//!   default, refuses more than 4300 digits;
//! - it keeps a `\ud800` escape without its pair as an unpaired surrogate; and
//! - it hands each object's members, duplicates included, to `object_pairs_hook` when the object
//!   closes, and each integer to `parse_int` when it reads the integer, so the errors that these
//!   hooks raise surface in document order. [`Hooks`] are their equivalents, and [`Loads`] are
//!   those of `json.loads` without arguments.
//!
//! [`read`] ports the C scanner of CPython 3.14, `scan_once_unicode` and the functions that it
//! calls in `Modules/_json.c`, with the checks of `JSONDecoder.decode` around it, so a document
//! that it refuses fails with the message and at the position of the `JSONDecodeError` that
//! Python raises, which [`syntax_error`] formats as Python does.
//!
//! The reader keeps values in an arena and never recurses, so it reads documents nested to any
//! depth. Python's decoder recurses and raises `RecursionError` at a depth that depends on the
//! Python version and the stack, so documents nested that deep are the one input on which the two
//! differ.
//!
//! # Why not `serde_json`
//!
//! Delegated commands keep Python's messages, which depend on the documents that the reader
//! accepts. A reader that refuses more documents, or reads a value with another type, changes
//! those messages, and `serde_json` does both, whatever its visitor:
//!
//! - it refuses `NaN`, `Infinity`, numbers beyond the range of a float, unpaired surrogates, and,
//!   by default, documents nested 128 levels deep; and
//! - it reads `-0` and integers beyond 64 bits as floats, and refuses integers beyond the range of
//!   a float.
//!
//! Its `arbitrary_precision` feature keeps numbers as written, but not the rest, and it would
//! change `serde_json::Number` for every crate built with `nvx`. The egress policy compiler, whose
//! example this was, reads its files as strict JSON with `serde_json` since microsoft/nvx#729.
//!
//! Other messages show the `JSONDecodeError` itself. `nvx.py sandbox`, for example, reports a
//! distro manifest that `json.loads` refuses with the error's message and position, as in
//! `invalid sandbox distro manifest: Expecting value: line 1 column 1 (char 0)`. `serde_json` words
//! its errors otherwise, and reports some at other positions, such as an unterminated string at its
//! end rather than its opening quote, so those messages need [`read`]'s port of Python's scanner.

use super::builtins::{Int, IntError};
use super::text::PyText;

/// Index of a value in a [`Document`].
pub(crate) type NodeId = usize;

/// A JSON value, as Python's decoder represents it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Node {
    /// `null`, Python's `None`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// An integer, as written, which is what `parse_int` receives.
    Integer(Box<str>),
    /// A number with a fraction or an exponent, or one of `NaN`, `Infinity`, and `-Infinity`.
    Float(f64),
    /// A string.
    String(PyText),
    /// An array of values.
    Array(Vec<NodeId>),
    /// An object's members, in document order, including any duplicate names.
    Object(Vec<(PyText, NodeId)>),
}

/// A JSON document: its values and its top-level value.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Document {
    /// Every value of the document; containers refer to their elements by index.
    nodes: Vec<Node>,
    /// Index of the top-level value.
    root: NodeId,
}

impl Document {
    /// Returns the top-level value.
    pub(crate) fn root(&self) -> NodeId {
        self.root
    }

    /// Returns the value at `id`, which a container of this document holds or [`root`](Self::root)
    /// returned.
    pub(crate) fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }

    /// Returns `value.get(name)` for the `dict` that Python builds from the object at `id`, in
    /// which the last of repeated names wins, or `None` if the value at `id` is not an object.
    pub(crate) fn get(&self, id: NodeId, name: &str) -> Option<&Node> {
        let Node::Object(members) = self.node(id) else {
            return None;
        };
        members
            .iter()
            .rev()
            .find(|(member, _)| member.as_bytes() == name.as_bytes())
            .map(|&(_, value)| self.node(value))
    }
}

/// Checks that run while a document is read, as Python's `object_pairs_hook` and `parse_int`
/// do. Their errors end the read.
pub(crate) trait Hooks {
    /// Error of a failed check.
    type Error;

    /// Runs when an object closes, with its members in document order.
    fn object(&mut self, members: &[(PyText, NodeId)]) -> Result<(), Self::Error> {
        let _ = members;
        Ok(())
    }

    /// Runs when an integer is read, with the integer as written, such as `-12`.
    fn integer(&mut self, literal: &str) -> Result<(), Self::Error> {
        let _ = literal;
        Ok(())
    }
}

/// The hooks of `json.loads` without arguments: `int`, the default `parse_int`, refuses an
/// integer of more than 4300 digits with a `ValueError` that [`IntError::message`] gives.
pub(crate) struct Loads;

impl Hooks for Loads {
    type Error = IntError;

    fn integer(&mut self, literal: &str) -> Result<(), IntError> {
        Int::parse(literal).map(drop)
    }
}

/// Why [`read`] failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReadError<E> {
    /// The text is not a document that Python's decoder accepts: Python raises a
    /// `JSONDecodeError` with this message, at the character at this byte offset.
    Syntax {
        /// The message of the `JSONDecodeError`, such as `Expecting value`.
        message: &'static str,
        /// Byte offset in the text of the character at which Python reports the error.
        offset: usize,
    },
    /// A hook failed.
    Hook(E),
}

/// Returns `str()` of the `JSONDecodeError` that Python raises with `message` at byte `offset`
/// of `text`: the message, the line and column of the position, and the position itself, in
/// characters, such as `Expecting value: line 1 column 2 (char 1)`.
pub(crate) fn syntax_error(text: &str, message: &str, offset: usize) -> String {
    let before = &text[..offset];
    let position = before.chars().count();
    let line = before.matches('\n').count() + 1;
    let column = match before.rfind('\n') {
        Some(newline) => before[newline + 1..].chars().count() + 1,
        None => position + 1,
    };
    format!("{message}: line {line} column {column} (char {position})")
}

/// Reads `text` as `json.loads(text)` does, running `hooks` as the document is read.
pub(crate) fn read<H: Hooks>(text: &str, hooks: &mut H) -> Result<Document, ReadError<H::Error>> {
    // Python refuses a string that starts with a byte-order mark before decoding it.
    if text.starts_with('\u{FEFF}') {
        return Err(ReadError::Syntax {
            message: "Unexpected UTF-8 BOM (decode using utf-8-sig)",
            offset: 0,
        });
    }
    let mut scanner = Scanner {
        text,
        bytes: text.as_bytes(),
        nodes: Vec::new(),
        hooks,
    };
    let start = scanner.skip_whitespace(0);
    let (root, end) = scanner.scan(start).map_err(|failure| match failure {
        Failure::Stop(offset) => ReadError::Syntax {
            message: "Expecting value",
            offset,
        },
        Failure::Decode(message, offset) => ReadError::Syntax { message, offset },
        Failure::Hook(error) => ReadError::Hook(error),
    })?;
    let end = scanner.skip_whitespace(end);
    if end != text.len() {
        return Err(ReadError::Syntax {
            message: "Extra data",
            offset: end,
        });
    }
    Ok(Document {
        nodes: scanner.nodes,
        root,
    })
}

/// Why scanning stopped.
enum Failure<E> {
    /// No value starts at this byte offset: the scanner's `StopIteration`.
    Stop(usize),
    /// A `JSONDecodeError` with this message at this byte offset.
    Decode(&'static str, usize),
    /// A hook failed.
    Hook(E),
}

/// An array or object whose values are being scanned.
enum Frame {
    /// An array, by its node.
    Array(NodeId),
    /// An object, by its node, with the name of the member whose value is being scanned.
    Object(NodeId, PyText),
}

/// The state of [`read`].
struct Scanner<'a, H> {
    /// The text being read.
    text: &'a str,
    /// The bytes of `text`.
    bytes: &'a [u8],
    /// The values read so far.
    nodes: Vec<Node>,
    /// The checks to run.
    hooks: &'a mut H,
}

impl<H: Hooks> Scanner<'_, H> {
    /// Returns the byte at `index`, if any.
    fn at(&self, index: usize) -> Option<u8> {
        self.bytes.get(index).copied()
    }

    /// Returns the offset of the first byte from `index` that is not whitespace that Python's
    /// decoder skips: a space, a tab, a line feed, or a carriage return.
    fn skip_whitespace(&self, mut index: usize) -> usize {
        while matches!(self.at(index), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            index += 1;
        }
        index
    }

    /// Adds a value to the document and returns its index.
    fn push(&mut self, node: Node) -> NodeId {
        self.nodes.push(node);
        self.nodes.len() - 1
    }

    /// Runs the object hook on the members of the object at `node`, which has just closed.
    fn close_object(&mut self, node: NodeId) -> Result<(), Failure<H::Error>> {
        if let Node::Object(members) = &self.nodes[node] {
            self.hooks.object(members).map_err(Failure::Hook)?;
        }
        Ok(())
    }

    /// Scans the value at `index`, returning its node and the offset after it, as `scan_once`
    /// does, without recursion.
    fn scan(&mut self, mut index: usize) -> Result<(NodeId, usize), Failure<H::Error>> {
        let mut frames: Vec<Frame> = Vec::new();
        loop {
            // Scan one value, or open an array or object.
            let mut value = None;
            let Some(first) = self.at(index) else {
                return Err(Failure::Stop(index));
            };
            match first {
                b'"' => {
                    let (string, next) = self.scan_string(index + 1)?;
                    value = Some(self.push(Node::String(string)));
                    index = next;
                }
                b'[' => {
                    let node = self.push(Node::Array(Vec::new()));
                    index = self.skip_whitespace(index + 1);
                    if self.at(index) == Some(b']') {
                        value = Some(node);
                        index += 1;
                    } else {
                        frames.push(Frame::Array(node));
                    }
                }
                b'{' => {
                    let node = self.push(Node::Object(Vec::new()));
                    index = self.skip_whitespace(index + 1);
                    if self.at(index) == Some(b'}') {
                        self.close_object(node)?;
                        value = Some(node);
                        index += 1;
                    } else {
                        let (name, next) = self.scan_name(index)?;
                        frames.push(Frame::Object(node, name));
                        index = next;
                    }
                }
                _ => {
                    let (node, next) = self.scan_scalar(index)?;
                    value = Some(self.push(node));
                    index = next;
                }
            }
            // Attach finished values to their arrays and objects.
            while let Some(finished) = value.take() {
                let Some(frame) = frames.last_mut() else {
                    return Ok((finished, index));
                };
                index = self.skip_whitespace(index);
                let (node, closing) = match frame {
                    Frame::Array(node) => (*node, b']'),
                    Frame::Object(node, _) => (*node, b'}'),
                };
                match (&mut self.nodes[node], frame) {
                    (Node::Array(values), Frame::Array(_)) => values.push(finished),
                    (Node::Object(members), Frame::Object(_, name)) => {
                        members.push((std::mem::take(name), finished));
                    }
                    _ => unreachable!("frames refer to their own arrays and objects"),
                }
                if self.at(index) == Some(closing) {
                    frames.pop();
                    if closing == b'}' {
                        self.close_object(node)?;
                    }
                    value = Some(node);
                    index += 1;
                    continue;
                }
                if self.at(index) != Some(b',') {
                    return Err(Failure::Decode("Expecting ',' delimiter", index));
                }
                let comma = index;
                index = self.skip_whitespace(index + 1);
                if self.at(index) == Some(closing) {
                    return Err(Failure::Decode(
                        if closing == b']' {
                            "Illegal trailing comma before end of array"
                        } else {
                            "Illegal trailing comma before end of object"
                        },
                        comma,
                    ));
                }
                if let Some(Frame::Object(_, name)) = frames.last_mut() {
                    let (next_name, next) = self.scan_name(index)?;
                    *name = next_name;
                    index = next;
                }
            }
        }
    }

    /// Scans a member's name at `index`, its colon, and the whitespace after them, returning the
    /// name and the offset of its value.
    fn scan_name(&self, index: usize) -> Result<(PyText, usize), Failure<H::Error>> {
        if self.at(index) != Some(b'"') {
            return Err(Failure::Decode(
                "Expecting property name enclosed in double quotes",
                index,
            ));
        }
        let (name, next) = self.scan_string(index + 1)?;
        let index = self.skip_whitespace(next);
        if self.at(index) != Some(b':') {
            return Err(Failure::Decode("Expecting ':' delimiter", index));
        }
        Ok((name, self.skip_whitespace(index + 1)))
    }

    /// Scans a constant or a number at `index`. Python matches the constants as prefixes, so
    /// whatever follows one is scanned separately.
    fn scan_scalar(&mut self, index: usize) -> Result<(Node, usize), Failure<H::Error>> {
        for (literal, node) in [
            ("null", Node::Null),
            ("true", Node::Bool(true)),
            ("false", Node::Bool(false)),
            ("NaN", Node::Float(f64::NAN)),
            ("Infinity", Node::Float(f64::INFINITY)),
            ("-Infinity", Node::Float(f64::NEG_INFINITY)),
        ] {
            if self.bytes[index..].starts_with(literal.as_bytes()) {
                return Ok((node, index + literal.len()));
            }
        }
        self.scan_number(index)
    }

    /// Scans a number at `start` as `_match_number_unicode` does: an optional `-`, `0` or digits
    /// without a leading zero, a fraction if a digit follows the point, and an exponent if a
    /// digit follows the `e` and its optional sign. A number with a fraction or an exponent is a
    /// float, and any other is an integer, which the integer hook receives.
    fn scan_number(&mut self, start: usize) -> Result<(Node, usize), Failure<H::Error>> {
        let digit = |index: usize| self.at(index).is_some_and(|byte| byte.is_ascii_digit());
        let last = self.bytes.len().saturating_sub(1);
        let mut index = start;
        if self.at(index) == Some(b'-') {
            index += 1;
            if index > last {
                return Err(Failure::Stop(start));
            }
        }
        match self.at(index) {
            Some(b'1'..=b'9') => {
                index += 1;
                while digit(index) {
                    index += 1;
                }
            }
            Some(b'0') => index += 1,
            _ => return Err(Failure::Stop(start)),
        }
        let mut float = false;
        if index < last && self.at(index) == Some(b'.') && digit(index + 1) {
            float = true;
            index += 2;
            while digit(index) {
                index += 1;
            }
        }
        if index < last && matches!(self.at(index), Some(b'e' | b'E')) {
            let exponent = index;
            index += 1;
            if index < last && matches!(self.at(index), Some(b'-' | b'+')) {
                index += 1;
            }
            while digit(index) {
                index += 1;
            }
            if digit(index - 1) {
                float = true;
            } else {
                index = exponent;
            }
        }
        let literal = &self.text[start..index];
        if float {
            // Like Python's `float`, Rust rounds correctly and overflows to an infinity, so any
            // literal of this form parses.
            return Ok((Node::Float(literal.parse().unwrap_or(f64::NAN)), index));
        }
        self.hooks.integer(literal).map_err(Failure::Hook)?;
        Ok((Node::Integer(literal.into()), index))
    }

    /// Scans a string whose opening quote precedes `end`, as `scanstring_unicode` does in strict
    /// mode, returning it and the offset after its closing quote: control characters are refused,
    /// and a `\u` escape of a high surrogate followed by a `\u` escape of a low surrogate is one
    /// character, while any other surrogate escape stays an unpaired surrogate.
    fn scan_string(&self, mut end: usize) -> Result<(PyText, usize), Failure<H::Error>> {
        let length = self.bytes.len();
        let begin = end - 1;
        let mut decoded = PyText::new();
        loop {
            // Find the end of the string or the next escape.
            let mut next = end;
            let mut found = None;
            while next < length {
                let byte = self.bytes[next];
                if byte == b'"' || byte == b'\\' {
                    found = Some(byte);
                    break;
                }
                if byte <= 0x1F {
                    return Err(Failure::Decode("Invalid control character at", next));
                }
                next += 1;
            }
            let Some(terminator) = found else {
                return Err(Failure::Decode("Unterminated string starting at", begin));
            };
            decoded.push_str(&self.text[end..next]);
            next += 1;
            if terminator == b'"' {
                return Ok((decoded, next));
            }
            let Some(escape) = self.at(next) else {
                return Err(Failure::Decode("Unterminated string starting at", begin));
            };
            if escape != b'u' {
                end = next + 1;
                let character = match escape {
                    b'"' | b'\\' | b'/' => char::from(escape),
                    b'b' => '\u{8}',
                    b'f' => '\u{c}',
                    b'n' => '\n',
                    b'r' => '\r',
                    b't' => '\t',
                    _ => return Err(Failure::Decode("Invalid \\escape", end - 2)),
                };
                decoded.push_code_point(u32::from(character));
                continue;
            }
            next += 1;
            end = next + 4;
            if end > length {
                return Err(Failure::Decode("Invalid \\uXXXX escape", next - 1));
            }
            let mut code_point = self
                .hex4(next)
                .ok_or(Failure::Decode("Invalid \\uXXXX escape", end - 5))?;
            // A high surrogate and a low-surrogate escape after it form one character.
            if (0xD800..=0xDBFF).contains(&code_point)
                && end + 6 < length
                && self.at(end) == Some(b'\\')
                && self.at(end + 1) == Some(b'u')
            {
                let low = self
                    .hex4(end + 2)
                    .ok_or(Failure::Decode("Invalid \\uXXXX escape", end + 1))?;
                if (0xDC00..=0xDFFF).contains(&low) {
                    code_point = 0x10000 + (((code_point - 0xD800) << 10) | (low - 0xDC00));
                    end += 6;
                }
            }
            decoded.push_code_point(code_point);
        }
    }

    /// Returns the value of the four hexadecimal digits at `index`, of either case.
    fn hex4(&self, index: usize) -> Option<u32> {
        self.bytes
            .get(index..index + 4)?
            .iter()
            .try_fold(0, |value, &digit| {
                Some((value << 4) | char::from(digit).to_digit(16)?)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hooks that refuse duplicate names and integers of more than 64 digits, as the egress
    /// policy reader does.
    struct Strict;

    impl Hooks for Strict {
        type Error = String;

        fn object(&mut self, members: &[(PyText, NodeId)]) -> Result<(), String> {
            let mut seen = std::collections::HashSet::new();
            for (name, _) in members {
                if !seen.insert(name) {
                    return Err(format!("duplicate {}", name.render()));
                }
            }
            Ok(())
        }

        fn integer(&mut self, literal: &str) -> Result<(), String> {
            if literal.trim_start_matches('-').len() > 64 {
                return Err("long integer".to_owned());
            }
            Ok(())
        }
    }

    /// Reads `text` with [`Strict`] hooks and describes the result.
    fn describe(text: &str) -> String {
        match read(text, &mut Strict) {
            Ok(document) => render(&document, document.root()),
            Err(ReadError::Syntax { .. }) => "syntax".to_owned(),
            Err(ReadError::Hook(message)) => message,
        }
    }

    /// Renders a value compactly, with the type of each scalar.
    fn render(document: &Document, id: NodeId) -> String {
        match document.node(id) {
            Node::Null => "null".to_owned(),
            Node::Bool(value) => value.to_string(),
            Node::Integer(literal) => format!("int:{literal}"),
            Node::Float(value) => format!("float:{value:?}"),
            Node::String(text) => format!("str:{}", text.render()),
            Node::Array(elements) => format!(
                "[{}]",
                elements
                    .iter()
                    .map(|&element| render(document, element))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Node::Object(members) => format!(
                "{{{}}}",
                members
                    .iter()
                    .map(|(name, value)| format!("{}={}", name.render(), render(document, *value)))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        }
    }

    #[test]
    fn values_read_as_python_reads_them() {
        for (text, expected) in [
            (
                " \t\n\r{\"a\": [1, -0, 2.5, 1e5, 1E+5, 1e-400, 1e400]}\n",
                "{a=[int:1,int:-0,float:2.5,float:100000.0,float:100000.0,float:0.0,float:inf]}",
            ),
            (
                "[NaN,Infinity,-Infinity]",
                "[float:NaN,float:inf,float:-inf]",
            ),
            ("[true,false,null]", "[true,false,null]"),
            (
                "\"\\u0031\\/\\b\\f\\n\\r\\t\\\"\\\\\"",
                "str:1/\u{8}\u{c}\n\r\t\"\\",
            ),
            ("\"\\ud83d\\ude00\"", "str:\u{1F600}"),
            ("\"\\ud800\"", "str:\\ud800"),
            ("\"\\udc00\\ud800\"", "str:\\udc00\\ud800"),
            ("\"\\ud800\\u0041\"", "str:\\ud800A"),
            ("\"\\ud800x\\udc00\"", "str:\\ud800x\\udc00"),
            ("\"\u{7F}é\u{2028}\"", "str:\u{7F}é\u{2028}"),
            ("{}", "{}"),
            ("[]", "[]"),
            ("[[[]]]", "[[[]]]"),
            ("{\"b\":1,\"a\":{\"c\":[]}}", "{b=int:1,a={c=[]}}"),
            ("-0.0", "float:-0.0"),
            ("0", "int:0"),
        ] {
            assert_eq!(describe(text), expected, "{text:?}");
        }
    }

    #[test]
    fn malformed_documents_are_refused() {
        for text in [
            "",
            " ",
            "\u{FEFF}{}",
            "{} x",
            "{}\u{C}",
            "[1,]",
            "{\"a\":1,}",
            "[01]",
            "[1.]",
            "[.5]",
            "[1e]",
            "[1e+]",
            "[-]",
            "[-a]",
            "[+1]",
            "[nul]",
            "[Nan]",
            "[-Inf]",
            "[inf]",
            "\"a",
            "\"a\\",
            "\"\\x\"",
            "\"\\u12\"",
            "\"\\u12g4\"",
            "\"\\ud800\\u12g4\"",
            "\"tab\there\"",
            "\"\u{0}\"",
            "{\"a\" 1}",
            "{a:1}",
            "{'a':1}",
            "[1 2]",
            "nullx",
            "truefalse",
            "[\"a\"\"b\"]",
            "{\"a\":1 \"b\":2}",
            "[",
            "{",
            "{\"a\":",
            "[1,",
        ] {
            assert_eq!(describe(text), "syntax", "{text:?}");
        }
    }

    #[test]
    fn hooks_run_in_document_order() {
        let long = "9".repeat(65);
        // The duplicate name is found when its object closes, after the long integer.
        assert_eq!(
            describe(&format!("{{\"a\":1,\"a\":2,\"b\":{long}}}")),
            "long integer"
        );
        // An inner object closes before the integer that follows it.
        assert_eq!(
            describe(&format!("{{\"a\":{{\"x\":1,\"x\":2}},\"b\":{long}}}")),
            "duplicate x"
        );
        // The object hook runs before trailing text is refused, and a syntax error before the
        // object closes prevails.
        assert_eq!(describe("{\"a\":1,\"a\":2} x"), "duplicate a");
        assert_eq!(describe("{\"a\":1,\"a\":2,}"), "syntax");
        // The first duplicate in document order is reported.
        assert_eq!(describe("{\"b\":1,\"a\":2,\"a\":3,\"b\":4}"), "duplicate a");
        // The integer hook runs before what follows the integer is checked.
        assert_eq!(describe(&format!("[{long}x]")), "long integer");
        assert_eq!(
            describe(&format!("[-{}]", "9".repeat(64))),
            format!("[int:-{}]", "9".repeat(64))
        );
        // Escaped names are compared after decoding.
        assert_eq!(describe("{\"a\":1,\"\\u0061\":2}"), "duplicate a");
        assert_eq!(
            describe("{\"\\ud800\":1,\"\\ud800\":2}"),
            "duplicate \\ud800"
        );
    }

    #[test]
    fn deep_nesting_needs_no_recursion() {
        let depth = 500_000;
        let text = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        let document = read(&text, &mut Strict).unwrap();
        assert!(
            matches!(document.node(document.root()), Node::Array(elements) if elements.len() == 1)
        );
        let text = format!("{}1{}", "{\"a\":".repeat(depth), "}".repeat(depth));
        read(&text, &mut Strict).unwrap();
    }

    /// Reads `text` as `json.loads(text)` does and describes its error as Python prints it.
    fn error(text: &str) -> String {
        match read(text, &mut Loads) {
            Ok(_) => panic!("{text:?} was read"),
            Err(ReadError::Syntax { message, offset }) => syntax_error(text, message, offset),
            Err(ReadError::Hook(error)) => error.message(text),
        }
    }

    #[test]
    fn documents_read_as_json_loads_reads_them() {
        let text = " [\"a\", 1, -2.5e1, NaN, -Infinity, null, true, {\"k\": [], \"k\": {}}] ";
        let document = read(text, &mut Loads).unwrap();
        let Node::Array(values) = document.node(document.root()) else {
            panic!("not an array");
        };
        let nodes: Vec<&Node> = values.iter().map(|&id| document.node(id)).collect();
        assert_eq!(nodes[0], &Node::String(PyText::from("a")));
        assert_eq!(nodes[1], &Node::Integer("1".into()));
        assert_eq!(nodes[2], &Node::Float(-25.0));
        assert!(matches!(nodes[3], Node::Float(value) if value.is_nan()));
        assert_eq!(nodes[4], &Node::Float(f64::NEG_INFINITY));
        assert_eq!(nodes[5], &Node::Null);
        assert_eq!(nodes[6], &Node::Bool(true));
        // The last of repeated names wins, as in the `dict` that Python builds.
        assert_eq!(
            document.get(values[7], "k"),
            Some(&Node::Object(Vec::new()))
        );
        assert_eq!(document.get(values[7], "missing"), None);
        assert_eq!(document.get(values[0], "k"), None);
    }

    #[test]
    fn errors_read_as_python_reports_them() {
        for (text, expected) in [
            ("", "Expecting value: line 1 column 1 (char 0)"),
            ("[", "Expecting value: line 1 column 2 (char 1)"),
            ("[1", "Expecting ',' delimiter: line 1 column 3 (char 2)"),
            (
                "[1,]",
                "Illegal trailing comma before end of array: line 1 column 3 (char 2)",
            ),
            (
                "{\"a\":1,}",
                "Illegal trailing comma before end of object: line 1 column 7 (char 6)",
            ),
            (
                "{1:2}",
                "Expecting property name enclosed in double quotes: line 1 column 2 (char 1)",
            ),
            (
                "{\"a\" 1}",
                "Expecting ':' delimiter: line 1 column 6 (char 5)",
            ),
            (
                "\"abc",
                "Unterminated string starting at: line 1 column 1 (char 0)",
            ),
            (
                "\"a\tb\"",
                "Invalid control character at: line 1 column 3 (char 2)",
            ),
            ("\"\\x\"", "Invalid \\escape: line 1 column 2 (char 1)"),
            (
                "\"\\u12\"",
                "Invalid \\uXXXX escape: line 1 column 3 (char 2)",
            ),
            ("[]\n x", "Extra data: line 2 column 2 (char 4)"),
            (
                "\u{feff}[]",
                "Unexpected UTF-8 BOM (decode using utf-8-sig): line 1 column 1 (char 0)",
            ),
            ("-", "Expecting value: line 1 column 1 (char 0)"),
            ("01", "Extra data: line 1 column 2 (char 1)"),
            // Positions count characters, not bytes.
            (
                "[\"é\" x]",
                "Expecting ',' delimiter: line 1 column 6 (char 5)",
            ),
            (
                "{\"\u{1F600}\":\n 1 2}",
                "Expecting ',' delimiter: line 2 column 4 (char 9)",
            ),
            (
                &format!("[{}]", "1".repeat(4301)),
                "Exceeds the limit (4300 digits) for integer string conversion: value has 4301 \
                 digits; use sys.set_int_max_str_digits() to increase the limit",
            ),
        ] {
            assert_eq!(error(text), expected, "{text:?}");
        }
        assert!(read(&format!("[-{}]", "1".repeat(4300)), &mut Loads).is_ok());
    }
}
