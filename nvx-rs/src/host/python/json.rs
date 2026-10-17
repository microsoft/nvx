//! A JSON reader that accepts exactly what Python's `json.loads` accepts.
//!
//! Python's decoder differs from strict JSON parsers such as `serde_json`:
//!
//! - it reads the constants `NaN`, `Infinity`, and `-Infinity` as floats, and numbers beyond the
//!   range of a float as infinities;
//! - it reads integers of any length, which `parse_int` receives as written;
//! - it keeps a `\ud800` escape without its pair as an unpaired surrogate; and
//! - it hands each object's members, duplicates included, to `object_pairs_hook` when the object
//!   closes, and each integer to `parse_int` when it reads the integer, so the errors that these
//!   hooks raise surface in document order. [`Hooks`] are their equivalents.
//!
//! The reader keeps values in an arena and never recurses, so it reads documents nested to any
//! depth. Python's decoder recurses and raises `RecursionError` at a depth that depends on the
//! Python version and the stack, so documents nested that deep are the one input on which the two
//! differ.

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

    /// Returns the document that Python's decoder reads from the JSON text of `value`.
    ///
    /// A number with a fraction or an exponent is a float, and any other an integer, as in that
    /// text. The value has no repeated field to check, but its integers keep any length.
    pub(crate) fn from_value(value: &serde_json::Value) -> Self {
        use serde_json::Value;

        let mut nodes = vec![Node::Null];
        // Each value and the index reserved for it, which its container already holds.
        let mut pending = vec![(value, 0)];
        while let Some((value, id)) = pending.pop() {
            let mut reserve = |value| {
                nodes.push(Node::Null);
                let id = nodes.len() - 1;
                pending.push((value, id));
                id
            };
            let node = match value {
                Value::Null => Node::Null,
                Value::Bool(value) => Node::Bool(*value),
                Value::Number(number) => {
                    // `serde_json` writes integers as digits, and other numbers with a fraction
                    // or an exponent, also with its `arbitrary_precision` feature.
                    let literal = number.to_string();
                    if literal
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || byte == b'-')
                    {
                        Node::Integer(literal.into())
                    } else {
                        Node::Float(number.as_f64().unwrap_or(f64::NAN))
                    }
                }
                Value::String(text) => Node::String(PyText::from(text.as_str())),
                Value::Array(elements) => Node::Array(elements.iter().map(&mut reserve).collect()),
                Value::Object(members) => Node::Object(
                    members
                        .iter()
                        .map(|(name, value)| (PyText::from(name.as_str()), reserve(value)))
                        .collect(),
                ),
            };
            nodes[id] = node;
        }
        Self { nodes, root: 0 }
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

/// Why [`read`] failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReadError<E> {
    /// The text is not a document that Python's decoder accepts; reading stopped at this byte
    /// offset.
    Syntax {
        /// Byte offset in the text at which reading stopped.
        offset: usize,
    },
    /// A hook failed.
    Hook(E),
}

/// Reads `text` as `json.loads(text)` does, running `hooks` as the document is read.
pub(crate) fn read<H: Hooks>(text: &str, hooks: &mut H) -> Result<Document, ReadError<H::Error>> {
    // Python refuses a string that starts with a byte-order mark before decoding it.
    if text.starts_with('\u{FEFF}') {
        return Err(ReadError::Syntax { offset: 0 });
    }
    let mut reader = Reader {
        text,
        bytes: text.as_bytes(),
        position: 0,
        nodes: Vec::new(),
        hooks,
    };
    reader.skip_whitespace();
    // The containers that enclose the value being read, innermost last.
    let mut open: Vec<Container> = Vec::new();
    loop {
        let mut value = match reader.peek() {
            Some(b'{') => {
                reader.position += 1;
                reader.skip_whitespace();
                if reader.peek() == Some(b'}') {
                    reader.position += 1;
                    reader.close(Container::Object(Vec::new(), PyText::new()))?
                } else {
                    let name = reader.member_name()?;
                    open.push(Container::Object(Vec::new(), name));
                    continue;
                }
            }
            Some(b'[') => {
                reader.position += 1;
                reader.skip_whitespace();
                if reader.peek() == Some(b']') {
                    reader.position += 1;
                    reader.close(Container::Array(Vec::new()))?
                } else {
                    open.push(Container::Array(Vec::new()));
                    continue;
                }
            }
            _ => reader.scalar()?,
        };
        // Adds the value to its container, and closes the containers that end after it.
        loop {
            let Some(mut container) = open.pop() else {
                reader.skip_whitespace();
                if reader.position != reader.bytes.len() {
                    return Err(reader.syntax());
                }
                return Ok(Document {
                    nodes: reader.nodes,
                    root: value,
                });
            };
            let closing = match &mut container {
                Container::Array(elements) => {
                    elements.push(value);
                    b']'
                }
                Container::Object(members, name) => {
                    members.push((std::mem::take(name), value));
                    b'}'
                }
            };
            reader.skip_whitespace();
            match reader.peek() {
                Some(b',') => {
                    reader.position += 1;
                    reader.skip_whitespace();
                    if let Container::Object(_, name) = &mut container {
                        *name = reader.member_name()?;
                    }
                    open.push(container);
                    break;
                }
                Some(byte) if byte == closing => {
                    reader.position += 1;
                    value = reader.close(container)?;
                }
                _ => return Err(reader.syntax()),
            }
        }
    }
}

/// An array or object whose elements are being read.
enum Container {
    /// The elements read so far.
    Array(Vec<NodeId>),
    /// The members read so far, and the name of the member whose value is being read.
    Object(Vec<(PyText, NodeId)>, PyText),
}

/// The state of [`read`].
struct Reader<'a, H> {
    /// The text being read.
    text: &'a str,
    /// The bytes of `text`.
    bytes: &'a [u8],
    /// Byte offset of the next unread byte.
    position: usize,
    /// The values read so far.
    nodes: Vec<Node>,
    /// The checks to run.
    hooks: &'a mut H,
}

impl<H: Hooks> Reader<'_, H> {
    /// Returns the next unread byte.
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    /// Returns a syntax error at the current position.
    fn syntax(&self) -> ReadError<H::Error> {
        ReadError::Syntax {
            offset: self.position,
        }
    }

    /// Skips the whitespace that Python's decoder skips: spaces, tabs, and line feeds and
    /// carriage returns.
    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.position += 1;
        }
    }

    /// Adds a value to the document and returns its index.
    fn push(&mut self, node: Node) -> NodeId {
        self.nodes.push(node);
        self.nodes.len() - 1
    }

    /// Adds a container whose closing bracket was read, after running the object hook on an
    /// object.
    fn close(&mut self, container: Container) -> Result<NodeId, ReadError<H::Error>> {
        Ok(match container {
            Container::Array(elements) => self.push(Node::Array(elements)),
            Container::Object(members, _) => {
                self.hooks.object(&members).map_err(ReadError::Hook)?;
                self.push(Node::Object(members))
            }
        })
    }

    /// Reads a member's name, the `:` after it, and the whitespace around the `:`.
    fn member_name(&mut self) -> Result<PyText, ReadError<H::Error>> {
        if self.peek() != Some(b'"') {
            return Err(self.syntax());
        }
        let name = self.string()?;
        self.skip_whitespace();
        if self.peek() != Some(b':') {
            return Err(self.syntax());
        }
        self.position += 1;
        self.skip_whitespace();
        Ok(name)
    }

    /// Reads a string, a constant, or a number.
    fn scalar(&mut self) -> Result<NodeId, ReadError<H::Error>> {
        let rest = &self.bytes[self.position..];
        // Python matches the constants as prefixes, so whatever follows one is read separately.
        let (node, length) = match rest.first() {
            Some(b'"') => {
                let text = self.string()?;
                return Ok(self.push(Node::String(text)));
            }
            Some(b'n') if rest.starts_with(b"null") => (Node::Null, 4),
            Some(b't') if rest.starts_with(b"true") => (Node::Bool(true), 4),
            Some(b'f') if rest.starts_with(b"false") => (Node::Bool(false), 5),
            Some(b'N') if rest.starts_with(b"NaN") => (Node::Float(f64::NAN), 3),
            Some(b'I') if rest.starts_with(b"Infinity") => (Node::Float(f64::INFINITY), 8),
            Some(b'-') if rest.starts_with(b"-Infinity") => (Node::Float(f64::NEG_INFINITY), 9),
            _ => return self.number(),
        };
        self.position += length;
        Ok(self.push(node))
    }

    /// Reads a number as the C scanner's `_match_number_unicode` does: an optional `-`, `0` or
    /// digits without a leading zero, a fraction if a digit follows the point, and an exponent if
    /// a digit follows the `e` and its optional sign. A number with a fraction or an exponent is a
    /// float, and any other is an integer, which the integer hook receives.
    fn number(&mut self) -> Result<NodeId, ReadError<H::Error>> {
        let bytes = self.bytes;
        let digit = |index: usize| bytes.get(index).is_some_and(u8::is_ascii_digit);
        let start = self.position;
        let mut end = start;
        if bytes.get(end) == Some(&b'-') {
            end += 1;
        }
        match bytes.get(end) {
            Some(b'1'..=b'9') => {
                end += 1;
                while digit(end) {
                    end += 1;
                }
            }
            Some(b'0') => end += 1,
            _ => return Err(self.syntax()),
        }
        let mut float = false;
        if bytes.get(end) == Some(&b'.') && digit(end + 1) {
            float = true;
            end += 2;
            while digit(end) {
                end += 1;
            }
        }
        if matches!(bytes.get(end), Some(b'e' | b'E')) {
            let mut exponent = end + 1;
            if matches!(bytes.get(exponent), Some(b'+' | b'-')) {
                exponent += 1;
            }
            if digit(exponent) {
                while digit(exponent) {
                    exponent += 1;
                }
                float = true;
                end = exponent;
            }
        }
        let literal = &self.text[start..end];
        let node = if float {
            // Like Python's `float`, Rust rounds correctly and overflows to an infinity, so any
            // literal of this form parses.
            Node::Float(literal.parse().map_err(|_| self.syntax())?)
        } else {
            self.hooks.integer(literal).map_err(ReadError::Hook)?;
            Node::Integer(literal.into())
        };
        self.position = end;
        Ok(self.push(node))
    }

    /// Reads a string whose opening quote is the next byte, as the C scanner's
    /// `scanstring_unicode` does: control characters are refused, and a `\u` escape of a high
    /// surrogate followed by a `\u` escape of a low surrogate is one character, while any other
    /// surrogate escape stays an unpaired surrogate.
    fn string(&mut self) -> Result<PyText, ReadError<H::Error>> {
        let bytes = self.bytes;
        let mut text = PyText::new();
        let mut index = self.position + 1;
        loop {
            let chunk = index;
            while let Some(&byte) = bytes.get(index) {
                if byte == b'"' || byte == b'\\' {
                    break;
                }
                if byte <= 0x1F {
                    self.position = index;
                    return Err(self.syntax());
                }
                index += 1;
            }
            text.push_str(&self.text[chunk..index]);
            match bytes.get(index) {
                Some(b'"') => {
                    self.position = index + 1;
                    return Ok(text);
                }
                Some(_) => index += 1,
                None => {
                    self.position = index;
                    return Err(self.syntax());
                }
            }
            // The escape after a backslash.
            let character = match bytes.get(index) {
                Some(b'"') => u32::from(b'"'),
                Some(b'\\') => u32::from(b'\\'),
                Some(b'/') => u32::from(b'/'),
                Some(b'b') => 0x08,
                Some(b'f') => 0x0C,
                Some(b'n') => u32::from(b'\n'),
                Some(b'r') => u32::from(b'\r'),
                Some(b't') => u32::from(b'\t'),
                Some(b'u') => {
                    let Some(mut code_point) = hex_escape(bytes, index + 1) else {
                        self.position = index;
                        return Err(self.syntax());
                    };
                    index += 4;
                    if (0xD800..=0xDBFF).contains(&code_point)
                        && bytes.get(index + 1..index + 3) == Some(b"\\u")
                    {
                        let Some(low) = hex_escape(bytes, index + 3) else {
                            self.position = index + 2;
                            return Err(self.syntax());
                        };
                        if (0xDC00..=0xDFFF).contains(&low) {
                            code_point = 0x10000 + ((code_point - 0xD800) << 10) + (low - 0xDC00);
                            index += 6;
                        }
                    }
                    code_point
                }
                _ => {
                    self.position = index;
                    return Err(self.syntax());
                }
            };
            text.push_code_point(character);
            index += 1;
        }
    }
}

/// Returns the value of the four hexadecimal digits at `start`, of either case.
fn hex_escape(bytes: &[u8], start: usize) -> Option<u32> {
    bytes
        .get(start..start + 4)?
        .iter()
        .try_fold(0, |value, &digit| {
            Some((value << 4) | char::from(digit).to_digit(16)?)
        })
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
}
