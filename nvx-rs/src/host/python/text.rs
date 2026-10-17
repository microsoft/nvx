//! Strings as Python holds them.
//!
//! A Python `str` is a sequence of code points, and unlike a Rust `str` it may hold unpaired
//! surrogates: Python's JSON decoder keeps a lone `\ud800` escape as is, and Python decodes
//! command-line paths that are not valid Unicode into surrogates. [`PyText`] keeps such strings
//! exactly, so the egress policy compiler compares and orders them as Python does, and
//! [`PyText::render`] shows them the way Python prints them.

use std::fmt;

/// A Python `str`: Unicode scalar values and unpaired surrogates.
///
/// Each code point is held in its UTF-8 form, a surrogate in the three-byte form that UTF-8 would
/// give it. Strings without surrogates are therefore plain UTF-8, and byte order is code-point
/// order, which is how Python orders strings.
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct PyText(Vec<u8>);

impl PyText {
    /// Returns an empty string.
    pub(crate) fn new() -> Self {
        Self(Vec::new())
    }

    /// Returns the bytes of the string: UTF-8, with surrogates in their three-byte form.
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Returns the string as a Rust string, or `None` if it holds a surrogate.
    pub(crate) fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }

    /// Appends a Rust string.
    pub(crate) fn push_str(&mut self, text: &str) {
        self.0.extend_from_slice(text.as_bytes());
    }

    /// Appends a code point, which may be a surrogate. `code_point` must not exceed `0x10FFFF`.
    pub(crate) fn push_code_point(&mut self, code_point: u32) {
        match char::from_u32(code_point) {
            Some(character) => {
                let mut buffer = [0; 4];
                self.push_str(character.encode_utf8(&mut buffer));
            }
            // Only surrogates, 0xD800 through 0xDFFF, are code points but not characters.
            None => self.0.extend_from_slice(&[
                0xE0 | (code_point >> 12) as u8,
                0x80 | ((code_point >> 6) & 0x3F) as u8,
                0x80 | (code_point & 0x3F) as u8,
            ]),
        }
    }

    /// Returns the code points of the string.
    pub(crate) fn code_points(&self) -> CodePoints<'_> {
        CodePoints(&self.0)
    }

    /// Returns the string as Python prints it to a UTF-8 stream, which escapes each surrogate as
    /// `\udXXX` with its `backslashreplace` error handler. Strings without surrogates are
    /// returned unchanged.
    pub(crate) fn render(&self) -> String {
        if let Some(text) = self.as_str() {
            return text.to_owned();
        }
        let mut rendered = String::with_capacity(self.0.len() + 8);
        for code_point in self.code_points() {
            match char::from_u32(code_point) {
                Some(character) => rendered.push(character),
                None => rendered.push_str(&format!("\\u{code_point:04x}")),
            }
        }
        rendered
    }
}

impl From<&str> for PyText {
    fn from(text: &str) -> Self {
        Self(text.as_bytes().to_vec())
    }
}

impl fmt::Debug for PyText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.render(), formatter)
    }
}

/// The code points of a [`PyText`], from first to last.
pub(crate) struct CodePoints<'a>(&'a [u8]);

impl Iterator for CodePoints<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        let (&lead, rest) = self.0.split_first()?;
        // A `PyText` holds well-formed sequences only, so the lead byte gives the length.
        let (length, initial) = match lead {
            0x00..=0x7F => (1, u32::from(lead)),
            0xC0..=0xDF => (2, u32::from(lead & 0x1F)),
            0xE0..=0xEF => (3, u32::from(lead & 0x0F)),
            _ => (4, u32::from(lead & 0x07)),
        };
        let continuation = &rest[..length - 1];
        self.0 = &rest[length - 1..];
        Some(continuation.iter().fold(initial, |code_point, byte| {
            (code_point << 6) | u32::from(byte & 0x3F)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surrogates_render_as_python_escapes() {
        let mut text = PyText::from("a");
        text.push_code_point(0xD800);
        text.push_code_point(0x1F600);
        text.push_code_point(0xDFFF);
        text.push_code_point(u32::from('é'));
        assert_eq!(text.as_str(), None);
        assert_eq!(text.render(), "a\\ud800\u{1F600}\\udfffé");
        assert_eq!(
            text.code_points().collect::<Vec<_>>(),
            [0x61, 0xD800, 0x1F600, 0xDFFF, 0xE9]
        );
        assert_eq!(PyText::from("plain").render(), "plain");
        assert_eq!(PyText::from("plain").as_str(), Some("plain"));
    }

    #[test]
    fn strings_order_by_code_point() {
        let surrogate = {
            let mut text = PyText::new();
            text.push_code_point(0xD800);
            text
        };
        // Python orders `'\ud7ff' < '\ud800' < '\ue000' < '\U00010000'`.
        let mut texts = vec![
            PyText::from("\u{10000}"),
            PyText::from("\u{E000}"),
            surrogate.clone(),
            PyText::from("\u{D7FF}"),
            PyText::from("b"),
            PyText::from("ab"),
            PyText::from("a"),
        ];
        texts.sort();
        assert_eq!(
            texts,
            [
                PyText::from("a"),
                PyText::from("ab"),
                PyText::from("b"),
                PyText::from("\u{D7FF}"),
                surrogate,
                PyText::from("\u{E000}"),
                PyText::from("\u{10000}"),
            ]
        );
    }
}
