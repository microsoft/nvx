//! The adapter's probes: named checks that the host asks the guest's worker to run in a VM with a
//! layered root, and the facts with which the worker answers, which a scenario then checks.
//!
//! A [`Probe`] is UTF-8 text: the probe's name, then each of its arguments after one space. A name
//! starts with a lowercase ASCII letter, and holds lowercase ASCII letters, digits, and `-`, so
//! that it never reads as a command-line option, and an argument is text without whitespace or
//! control characters. The worker answers a probe that it ran with the [`Facts`] that it found,
//! and one that it does not know, or that failed, with the problem, as UTF-8 text.
//!
//! [`Facts`] are UTF-8 text too: one `KEY=VALUE` line per fact, each ended by a newline, in the
//! order in which the probe found them, and each key at most once. A key holds lowercase ASCII
//! letters, digits, `.`, and `-`, and a value [escapes](escape) `\`, newlines, and carriage
//! returns as `\\`, `\n`, and `\r`, so that each fact stays on one line.

use std::fmt;

use crate::frame::{BodyError, MAX_TEXT_BODY};

/// A probe: a named check that the guest's worker runs, with its arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    /// The probe's name.
    name: String,
    /// The probe's arguments.
    arguments: Vec<String>,
}

impl Probe {
    /// Returns the probe `name` with `arguments`.
    ///
    /// Fails with a [`BodyError`] if the name or an argument breaks the rules of the
    /// [module documentation](self), or if the probe's encoding is longer than
    /// [`MAX_TEXT_BODY`] bytes.
    pub fn new<S: Into<String>>(
        name: impl Into<String>,
        arguments: impl IntoIterator<Item = S>,
    ) -> Result<Self, BodyError> {
        let name = name.into();
        let valid_name = name.starts_with(|character: char| character.is_ascii_lowercase())
            && name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
        if !valid_name {
            return Err(BodyError::new(format!(
                "the probe's name {name:?} is not a lowercase ASCII letter, then lowercase ASCII \
                 letters, digits, and `-`"
            )));
        }
        let arguments = arguments.into_iter().map(Into::into).collect::<Vec<_>>();
        let invalid = arguments.iter().find(|argument| {
            argument.is_empty()
                || argument
                    .chars()
                    .any(|character| character.is_whitespace() || character.is_control())
        });
        if let Some(argument) = invalid {
            return Err(BodyError::new(format!(
                "the {name} probe's argument {argument:?} is empty, or holds whitespace or a \
                 control character"
            )));
        }
        let probe = Self { name, arguments };
        let length = probe.to_string().len();
        if length > MAX_TEXT_BODY {
            return Err(BodyError::new(format!(
                "the {} probe takes {length} bytes, more than {MAX_TEXT_BODY}",
                probe.name
            )));
        }
        Ok(probe)
    }

    /// Reads a probe from its text, its name and then each argument after one space, as a
    /// scenario names it.
    ///
    /// Fails as [`new`](Self::new) does.
    pub fn parse(text: &str) -> Result<Self, BodyError> {
        let mut words = text.split(' ');
        let name = words.next().unwrap_or_default();
        Self::new(name, words)
    }

    /// Returns the probe's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the probe's arguments.
    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }

    /// Returns the probe's encoding: its text.
    pub fn encode(&self) -> Vec<u8> {
        self.to_string().into_bytes()
    }

    /// Reads a probe from its encoding.
    ///
    /// Fails with a [`BodyError`] if the body is not UTF-8 text, and otherwise as
    /// [`parse`](Self::parse) does.
    pub fn decode(body: &[u8]) -> Result<Self, BodyError> {
        let text =
            std::str::from_utf8(body).map_err(|_| BodyError::new("the probe is not UTF-8 text"))?;
        Self::parse(text)
    }
}

impl fmt::Display for Probe {
    /// Writes the probe's text: its name, then each argument after one space.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.name)?;
        for argument in &self.arguments {
            write!(formatter, " {argument}")?;
        }
        Ok(())
    }
}

/// What a probe found: facts, each a key and a value, in the order in which the probe found them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts(Vec<(String, String)>);

impl Facts {
    /// Returns no facts.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds the fact that `key` has `value`.
    ///
    /// # Panics
    ///
    /// If `key` breaks the rules of the [module documentation](self), or if a fact has it
    /// already: a probe names its facts itself.
    pub fn push(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let key = key.into();
        assert!(valid_key(&key), "the fact's key {key:?} is not valid");
        assert!(self.get(&key).is_none(), "the fact {key} is repeated");
        self.0.push((key, value.into()));
    }

    /// Returns the value of the fact `key`, if any.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    /// Returns the facts, each as its key and its value, in order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    /// Returns the number of facts.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns whether there are no facts.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the facts' encoding: one `KEY=VALUE` line per fact, with the value
    /// [escaped](escape).
    pub fn encode(&self) -> Vec<u8> {
        self.iter()
            .flat_map(|(key, value)| format!("{key}={}\n", escape(value)).into_bytes())
            .collect()
    }

    /// Reads facts from their encoding.
    ///
    /// Fails with a [`BodyError`] that names the first line that breaks the rules of the
    /// [module documentation](self).
    pub fn decode(body: &[u8]) -> Result<Self, BodyError> {
        let text = std::str::from_utf8(body)
            .map_err(|_| BodyError::new("the facts are not UTF-8 text"))?;
        let mut facts = Self::new();
        if text.is_empty() {
            return Ok(facts);
        }
        let lines = text
            .strip_suffix('\n')
            .ok_or_else(|| BodyError::new("the facts do not end their last line with a newline"))?;
        for line in lines.split('\n') {
            let invalid = |problem: &str| BodyError::new(format!("the fact {line:?} {problem}"));
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| invalid("has no value"))?;
            if !valid_key(key) {
                return Err(invalid("has an invalid key"));
            }
            if facts.get(key).is_some() {
                return Err(invalid("repeats its key"));
            }
            let value = unescape(value).ok_or_else(|| invalid("has an invalid escape"))?;
            facts.0.push((key.to_owned(), value));
        }
        Ok(facts)
    }
}

/// Returns whether `key` is a valid key of a fact: lowercase ASCII letters, digits, `.`, and `-`.
pub(crate) fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'.' || byte == b'-'
        })
}

/// Returns `value` with `\`, newlines, and carriage returns escaped as `\\`, `\n`, and `\r`, as a
/// fact's encoding holds it, on one line.
pub fn escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            character => escaped.push(character),
        }
    }
    escaped
}

/// Reverses [`escape`], or returns `None` for a `\` that starts no escape that it writes.
fn unescape(escaped: &str) -> Option<String> {
    let mut value = String::with_capacity(escaped.len());
    let mut characters = escaped.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            value.push(character);
            continue;
        }
        value.push(match characters.next()? {
            '\\' => '\\',
            'n' => '\n',
            'r' => '\r',
            _ => return None,
        });
    }
    Some(value)
}

/// How the guest's worker answered a probe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeReply {
    /// The probe ran, and found these facts.
    Report(Facts),
    /// The worker does not know the probe, or the probe failed, as the text says.
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_probe_is_its_name_then_its_arguments() {
        let probe = Probe::new("hold", ["/run/a", "ä"]).unwrap();
        assert_eq!(probe.name(), "hold");
        assert_eq!(probe.arguments(), ["/run/a", "ä"]);
        assert_eq!(probe.encode(), "hold /run/a ä".as_bytes());
        assert_eq!(Probe::decode(&probe.encode()), Ok(probe.clone()));
        assert_eq!(Probe::parse("hold /run/a ä"), Ok(probe));
        let bare = Probe::parse("mounts").unwrap();
        assert_eq!((bare.name(), bare.arguments()), ("mounts", &[][..]));
        assert_eq!(bare.to_string(), "mounts");

        let longest = format!("p {}", "a".repeat(MAX_TEXT_BODY - 2));
        assert_eq!(
            Probe::parse(&longest).unwrap().encode().len(),
            MAX_TEXT_BODY
        );
        for (text, problem) in [
            ("", "name \"\" is not"),
            ("Mounts", "name \"Mounts\" is not"),
            ("mounts.all", "is not a lowercase ASCII letter, then"),
            ("-mounts", "name \"-mounts\" is not"),
            ("--mounts", "name \"--mounts\" is not"),
            ("9p", "name \"9p\" is not"),
            (" mounts", "name \"\" is not"),
            ("mounts ", "argument \"\" is empty"),
            ("mounts  a", "argument \"\" is empty"),
            ("mounts a\tb", "holds whitespace"),
            ("mounts a\u{7f}", "a control character"),
            ("mounts a\u{a0}b", "holds whitespace"),
            (&format!("{longest}a"), "more than 4096"),
        ] {
            let error = Probe::parse(text).unwrap_err();
            assert!(error.problem().contains(problem), "{text:?}: {error}");
        }
        let error = Probe::decode(b"mounts \xff").unwrap_err();
        assert_eq!(error.problem(), "the probe is not UTF-8 text");
    }

    #[test]
    fn facts_are_one_line_each_with_escaped_values() {
        let mut facts = Facts::new();
        assert!(facts.is_empty());
        facts.push("root.type", "overlay");
        facts.push("os-release.id", "a\\b\nc\rd=e");
        facts.push("empty", "");
        assert_eq!(facts.len(), 3);
        assert_eq!(facts.get("root.type"), Some("overlay"));
        assert_eq!(facts.get("missing"), None);
        assert_eq!(
            facts.iter().collect::<Vec<_>>(),
            [
                ("root.type", "overlay"),
                ("os-release.id", "a\\b\nc\rd=e"),
                ("empty", ""),
            ]
        );
        let encoded = facts.encode();
        assert_eq!(
            encoded,
            b"root.type=overlay\nos-release.id=a\\\\b\\nc\\rd=e\nempty=\n"
        );
        assert_eq!(Facts::decode(&encoded), Ok(facts));
        assert_eq!(Facts::decode(b""), Ok(Facts::new()));

        for (body, problem) in [
            (&b"a=\xff\n"[..], "are not UTF-8 text"),
            (b"a=b", "do not end their last line"),
            (b"a=b\n\n", "\"\" has no value"),
            (b"ab\n", "\"ab\" has no value"),
            (b"A=b\n", "has an invalid key"),
            (b"=b\n", "has an invalid key"),
            (b"a b=c\n", "has an invalid key"),
            (b"a=b\na=c\n", "repeats its key"),
            (b"a=b\\\n", "has an invalid escape"),
            (b"a=b\\t\n", "has an invalid escape"),
        ] {
            let error = Facts::decode(body).unwrap_err();
            assert!(error.problem().contains(problem), "{body:?}: {error}");
        }
    }

    #[test]
    #[should_panic(expected = "the fact a is repeated")]
    fn a_probe_names_each_fact_once() {
        let mut facts = Facts::new();
        facts.push("a", "1");
        facts.push("a", "2");
    }

    #[test]
    #[should_panic(expected = "is not valid")]
    fn a_probe_names_its_facts_with_valid_keys() {
        Facts::new().push("Key", "value");
    }
}
