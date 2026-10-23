//! Paths as Python's `pathlib` and `os.path` hold and print them.
//!
//! A [`PurePath`] holds a Unicode path of either [`Flavor`], as `PurePosixPath` or
//! `PureWindowsPath` does, with its parts, parents, joins, and case-insensitive comparison on
//! Windows. `str(path)` normalizes the path that it was given: it drops empty and `.` components,
//! and on Windows turns `/` into `\` and gives UNC shares a root. [`Flavor`] has the `os.path`
//! operations of `posixpath` and `ntpath` that the ports use, `splitroot`, `join`, `isabs`, and
//! `split`, and [`windows_normpath`] is `ntpath.normpath`.
//!
//! Windows paths follow CPython 3.14 on Windows, whose `ntpath.splitroot` and `ntpath.normpath`
//! are C functions that index UTF-16 code units: a character outside the Basic Multilingual Plane
//! followed by `:` is no drive there. Python's own `ntpath`, which `PureWindowsPath` uses on other
//! hosts, indexes code points instead, which differs only for such a character.

use std::fmt;
use std::path::Path;

/// Returns `str(PurePosixPath(path))` for a path given as bytes, which Python sees as the
/// characters that they decode to.
///
/// One leading `/`, or three or more, are a root of `/`, and exactly two are a root of `//`;
/// empty and `.` components are dropped, and an empty result is `.`.
#[cfg(test)]
fn posix_str(path: &[u8]) -> Vec<u8> {
    let (root, tail) = parse_posix(path);
    format_posix(root, &tail)
}

/// Returns `str(PureWindowsPath(path))` on Windows for a path given as UTF-16 code units, as
/// Python 3.14 computes it with `nt._path_splitroot_ex`, which indexes UTF-16 code units.
///
/// `/` becomes `\`; a drive is a letter or other code unit and a colon, a UNC share
/// (`\\server\share`, `\\?\UNC\server\share`), or a device (`\\.\device`, `\\?\device`); UNC
/// shares gain a root; empty and `.` components are dropped; a relative path whose first
/// component looks like a drive gains a `.\` prefix; and an empty result is `.`.
#[cfg(test)]
fn windows_str(path: &[u16]) -> Vec<u16> {
    let (drive, root, tail) = parse_windows(path);
    format_windows(&drive, &root, &tail)
}

/// The separator of Windows paths, `\`.
const SEPARATOR: u16 = b'\\' as u16;

/// Returns whether a code unit separates the components of a Windows path: `\` or `/`.
fn is_windows_separator(unit: u16) -> bool {
    unit == SEPARATOR || unit == u16::from(b'/')
}

/// Returns the length of the root of a POSIX path, as `posixpath.splitroot` finds it: exactly two
/// leading slashes are a root of their own, and one, or three or more, a root of `/`.
fn posix_root_length(path: &[u8]) -> usize {
    match path {
        [b'/', b'/', b'/', ..] => 1,
        [b'/', b'/', ..] => 2,
        [b'/', ..] => 1,
        _ => 0,
    }
}

/// Returns the root and the components of a POSIX path, as `pathlib` parses it: empty and `.`
/// components are dropped.
fn parse_posix(path: &[u8]) -> (&[u8], Vec<&[u8]>) {
    let root = posix_root_length(path);
    let tail = path[root..]
        .split(|&byte| byte == b'/')
        .filter(|part| !part.is_empty() && *part != b".")
        .collect();
    (&path[..root], tail)
}

/// Returns `str()` of the POSIX path of a root and components, as `pathlib` formats it.
fn format_posix(root: &[u8], tail: &[&[u8]]) -> Vec<u8> {
    let mut result = root.to_vec();
    for (index, part) in tail.iter().enumerate() {
        if index > 0 {
            result.push(b'/');
        }
        result.extend_from_slice(part);
    }
    if result.is_empty() {
        result.push(b'.');
    }
    result
}

/// Returns the drive, the root, and the components of a Windows path given as UTF-16 code units,
/// as `pathlib` parses it: `/` becomes `\`, a UNC share gains a root, and empty and `.`
/// components are dropped.
fn parse_windows(path: &[u16]) -> (Vec<u16>, Vec<u16>, Vec<Vec<u16>>) {
    let path: Vec<u16> = path
        .iter()
        .map(|&unit| {
            if is_windows_separator(unit) {
                SEPARATOR
            } else {
                unit
            }
        })
        .collect();
    let (drive_length, root_length) = windows_skip_root(&path);
    let drive = &path[..drive_length];
    let mut root = path[drive_length..drive_length + root_length].to_vec();
    if root.is_empty() && drive.first() == Some(&SEPARATOR) && drive.last() != Some(&SEPARATOR) {
        // `pathlib` gives a share, `\\server\share` or `\\?\UNC\server\share`, a root, but not a
        // device such as `\\.\device` or `\\?\device`. Its `not in '?.'` test is a substring
        // test, which the empty string also passes.
        let parts: Vec<&[u16]> = drive.split(|&unit| unit == SEPARATOR).collect();
        let device = |part: &[u16]| {
            part.is_empty()
                || part == [u16::from(b'?')]
                || part == [u16::from(b'.')]
                || part == [u16::from(b'?'), u16::from(b'.')]
        };
        if (parts.len() == 4 && !device(parts[2])) || parts.len() == 6 {
            root = vec![SEPARATOR];
        }
    }
    let tail = path[drive_length + root_length..]
        .split(|&unit| unit == SEPARATOR)
        .filter(|part| !part.is_empty() && *part != [u16::from(b'.')])
        .map(<[u16]>::to_vec)
        .collect();
    (drive.to_vec(), root, tail)
}

/// Returns `str()` of the Windows path of a drive, a root, and components, as `pathlib` formats
/// it: a relative path whose first component would read as a drive gains a `.\` prefix.
fn format_windows(drive: &[u16], root: &[u16], tail: &[Vec<u16>]) -> Vec<u16> {
    let mut result = Vec::new();
    if !drive.is_empty() || !root.is_empty() {
        result.extend_from_slice(drive);
        result.extend_from_slice(root);
    } else if tail
        .first()
        .is_some_and(|first| windows_skip_root(first).0 > 0)
    {
        // A first component such as `c:foo` would read as a drive.
        result.extend_from_slice(&[u16::from(b'.'), SEPARATOR]);
    }
    for (index, part) in tail.iter().enumerate() {
        if index > 0 {
            result.push(SEPARATOR);
        }
        result.extend_from_slice(part);
    }
    if result.is_empty() {
        result.push(u16::from(b'.'));
    }
    result
}

/// Returns the lengths in code units of the drive and the root of a Windows path, as CPython's
/// `_Py_skiproot` finds them on Windows, with `\` and `/` as separators.
fn windows_skip_root(path: &[u16]) -> (usize, usize) {
    let unit = |index: usize| path.get(index).copied();
    let separator = |index: usize| unit(index).is_some_and(is_windows_separator);
    let either = |index: usize, upper: u8, lower: u8| {
        unit(index) == Some(u16::from(upper)) || unit(index) == Some(u16::from(lower))
    };
    if separator(0) {
        if !separator(1) {
            // A root without a drive, such as `\Windows`.
            return (0, 1);
        }
        // A UNC share, such as `\\server\share` or `\\?\UNC\server\share`, or a device, such as
        // `\\.\device` or `\\?\device`.
        let unc = unit(2) == Some(u16::from(b'?'))
            && separator(3)
            && either(4, b'U', b'u')
            && either(5, b'N', b'n')
            && either(6, b'C', b'c')
            && separator(7);
        let mut index = if unc { 8 } else { 2 };
        while index < path.len() && !separator(index) {
            index += 1;
        }
        if index >= path.len() {
            return (path.len(), 0);
        }
        index += 1;
        while index < path.len() && !separator(index) {
            index += 1;
        }
        if index >= path.len() {
            (path.len(), 0)
        } else {
            (index, 1)
        }
    } else if !path.is_empty() && unit(1) == Some(u16::from(b':')) {
        // A drive, such as `X:`, and a root if a separator follows it.
        (2, usize::from(separator(2)))
    } else {
        (0, 0)
    }
}

/// Returns the string of bytes that come from a Unicode path cut at ASCII separators.
fn from_utf8(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Returns the string of code units that come from a Unicode path cut at ASCII separators.
fn from_utf16(units: &[u16]) -> String {
    String::from_utf16_lossy(units)
}

/// Returns the byte offset in `text` of the character that starts at UTF-16 code unit `units`.
fn utf16_offset(text: &str, units: usize) -> usize {
    let mut counted = 0;
    for (offset, character) in text.char_indices() {
        if counted >= units {
            return offset;
        }
        counted += character.len_utf16();
    }
    text.len()
}

/// The two flavors of `pathlib` paths: `PurePosixPath` and `PureWindowsPath`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Flavor {
    /// `posixpath`, with `/` as its only separator.
    Posix,
    /// `ntpath`, with `\` as its separator and `/` as an alternative.
    Windows,
}

impl Flavor {
    /// The flavor of `pathlib.Path` on this host.
    pub(crate) const NATIVE: Self = if cfg!(windows) {
        Self::Windows
    } else {
        Self::Posix
    };

    /// Returns whether `character` separates components: `os.sep` or `os.altsep`.
    pub(crate) fn is_separator(self, character: char) -> bool {
        character == '/' || (self == Self::Windows && character == '\\')
    }

    /// Returns `os.path.splitroot(path)`: its drive, root, and the rest, as slices of `path`.
    pub(crate) fn split_root(self, path: &str) -> (&str, &str, &str) {
        let (drive, root) = match self {
            Self::Posix => (0, posix_root_length(path.as_bytes())),
            Self::Windows => {
                let units: Vec<u16> = path.encode_utf16().collect();
                let (drive, root) = windows_skip_root(&units);
                let drive_end = utf16_offset(path, drive);
                (drive_end, utf16_offset(path, drive + root) - drive_end)
            }
        };
        (
            &path[..drive],
            &path[drive..drive + root],
            &path[drive + root..],
        )
    }

    /// Returns `os.path.join(first, second)`.
    pub(crate) fn join(self, first: &str, second: &str) -> String {
        match self {
            Self::Posix => {
                if second.starts_with('/') || first.is_empty() {
                    second.to_owned()
                } else if first.ends_with('/') {
                    format!("{first}{second}")
                } else {
                    format!("{first}/{second}")
                }
            }
            Self::Windows => windows_join(first, second),
        }
    }

    /// Returns `os.path.isabs(path)`. A Windows path is absolute with a drive and a root, or as
    /// a UNC or device path; `ntpath.isabs` tests the code points of the path, even on Windows.
    pub(crate) fn is_absolute(self, path: &str) -> bool {
        match self {
            Self::Posix => path.starts_with('/'),
            Self::Windows => {
                let start: String = path
                    .chars()
                    .take(3)
                    .map(|character| if character == '/' { '\\' } else { character })
                    .collect();
                start.starts_with("\\\\") || start.chars().skip(1).eq([':', '\\'])
            }
        }
    }

    /// Returns `os.path.split(path)`: the path without its last component, and that component.
    #[cfg(any(windows, test))]
    pub(crate) fn split(self, path: &str) -> (String, String) {
        match self {
            Self::Posix => {
                let index = path.rfind('/').map_or(0, |index| index + 1);
                let (head, tail) = path.split_at(index);
                // A head of slashes only is kept whole.
                let head = if head.chars().all(|character| character == '/') {
                    head
                } else {
                    head.trim_end_matches('/')
                };
                (head.to_owned(), tail.to_owned())
            }
            Self::Windows => {
                let (drive, root, rest) = self.split_root(path);
                let index = rest.rfind(['\\', '/']).map_or(0, |index| index + 1);
                let (head, tail) = rest.split_at(index);
                (
                    format!("{drive}{root}{}", head.trim_end_matches(['\\', '/'])),
                    tail.to_owned(),
                )
            }
        }
    }
}

/// Returns `ntpath.join(first, second)`.
fn windows_join(first: &str, second: &str) -> String {
    let is_separator = |character: char| character == '\\' || character == '/';
    let (drive, root, path) = Flavor::Windows.split_root(first);
    let (mut drive, mut root, mut path) = (drive.to_owned(), root.to_owned(), path.to_owned());
    let (second_drive, second_root, second_path) = Flavor::Windows.split_root(second);
    if !second_root.is_empty() {
        // The second path is absolute.
        if !second_drive.is_empty() || drive.is_empty() {
            second_drive.clone_into(&mut drive);
        }
        second_root.clone_into(&mut root);
        second_path.clone_into(&mut path);
    } else if !second_drive.is_empty()
        && second_drive != drive
        && second_drive.to_lowercase() != drive.to_lowercase()
    {
        // Another drive: the first path is dropped.
        second_drive.clone_into(&mut drive);
        second_root.clone_into(&mut root);
        second_path.clone_into(&mut path);
    } else {
        if !second_drive.is_empty() {
            // The same drive, in other case.
            second_drive.clone_into(&mut drive);
        }
        if path.chars().last().is_some_and(|last| !is_separator(last)) {
            path.push('\\');
        }
        path.push_str(second_path);
    }
    // A UNC drive and a relative path need a separator between them.
    if !path.is_empty()
        && root.is_empty()
        && drive
            .chars()
            .last()
            .is_some_and(|last| !matches!(last, ':' | '\\' | '/'))
    {
        return format!("{drive}\\{path}");
    }
    format!("{drive}{root}{path}")
}

/// Returns `ntpath.normpath(path)`, as CPython's `_Py_normpath_and_size` computes it on Windows:
/// separators become `\`, empty and `.` components disappear, and `..` removes the component
/// before it, except at the root or after another `..`.
#[cfg(any(windows, test))]
pub(crate) fn windows_normpath(path: &str) -> String {
    let input: Vec<char> = path.chars().collect();
    if input.is_empty() {
        return ".".to_owned();
    }
    let separator = |character: char| character == '\\' || character == '/';
    let is_separator_or_end = |index: usize| input.get(index).is_none_or(|&c| separator(c));
    let (drive, root, _) = Flavor::Windows.split_root(path);
    let prefix_length = path[..drive.len() + root.len()].chars().count();
    let mut output: Vec<char> = Vec::with_capacity(input.len());
    let mut read = 0;
    // The output index that `..` never removes, and the last character that was copied.
    let mut minimum = 0;
    let mut last = '\0';
    if prefix_length > 0 {
        output.extend(
            input[..prefix_length]
                .iter()
                .map(|&character| if character == '/' { '\\' } else { character }),
        );
        read = prefix_length;
        minimum = output.len() - 1;
        last = output[minimum];
        if last != '\\' {
            minimum += 1;
        }
    }
    if input.get(read) == Some(&'.') && is_separator_or_end(read + 1) {
        // Skip a leading `.\`.
        read += 1;
        last = match input.get(read) {
            Some('/') => '\\',
            Some(&character) => character,
            None => '\0',
        };
        while input
            .get(read)
            .is_some_and(|&character| separator(character))
        {
            read += 1;
        }
    }
    while read < input.len() {
        let character = match input[read] {
            '/' => '\\',
            character => character,
        };
        if last == '\\' {
            if character == '.' {
                let separator_at_1 = is_separator_or_end(read + 1);
                let separator_at_2 = !separator_at_1 && is_separator_or_end(read + 2);
                if separator_at_2 && input[read + 1] == '.' {
                    let mut start = output.len();
                    while start != minimum {
                        start -= 1;
                        if output[start] != '\\' {
                            break;
                        }
                    }
                    while start != minimum && output[start - 1] != '\\' {
                        start -= 1;
                    }
                    let previous_is_parent = output.get(start) == Some(&'.')
                        && output.get(start + 1) == Some(&'.')
                        && output.get(start + 2).is_some_and(|&c| separator(c));
                    if output.len() == minimum || previous_is_parent {
                        // Keep a `..` that has nothing to remove, or that follows another.
                        output.extend(['.', '.']);
                        last = '.';
                    } else if output.get(start) == Some(&'\\') {
                        // At the root of an absolute path, `..` removes the component.
                        output.truncate(start + 1);
                    } else {
                        output.truncate(start);
                    }
                    read += 1;
                } else if !separator_at_1 {
                    output.push(character);
                    last = character;
                }
            } else if character != '\\' {
                output.push(character);
                last = character;
            }
        } else {
            output.push(character);
            last = character;
        }
        read += 1;
    }
    // Drop trailing separators, but not the root.
    let mut length = output.len();
    if length != minimum {
        loop {
            length -= 1;
            if length == minimum || output[length] != '\\' {
                break;
            }
        }
        length += 1;
    }
    if length == 0 {
        return ".".to_owned();
    }
    output[..length].iter().collect()
}

/// A pure path of one [`Flavor`], as `PurePosixPath` or `PureWindowsPath` holds it.
#[derive(Clone)]
pub(crate) struct PurePath {
    /// The flavor of the path.
    flavor: Flavor,
    /// The drive: a drive letter and colon, or a UNC or device drive, on Windows; empty on POSIX.
    drive: String,
    /// The root: one separator, two slashes on POSIX, or empty for a relative path.
    root: String,
    /// The components after the anchor.
    tail: Vec<String>,
    /// `str(path)`.
    text: String,
}

impl PurePath {
    /// Returns the path of `text` in `flavor`, as `PurePosixPath(text)` or
    /// `PureWindowsPath(text)` does.
    pub(crate) fn new(flavor: Flavor, text: &str) -> Self {
        let (drive, root, tail) = match flavor {
            Flavor::Posix => {
                let (root, tail) = parse_posix(text.as_bytes());
                (
                    String::new(),
                    from_utf8(root),
                    tail.into_iter().map(from_utf8).collect(),
                )
            }
            Flavor::Windows => {
                let units: Vec<u16> = text.encode_utf16().collect();
                let (drive, root, tail) = parse_windows(&units);
                (
                    from_utf16(&drive),
                    from_utf16(&root),
                    tail.iter().map(|part| from_utf16(part)).collect(),
                )
            }
        };
        Self::from_parts(flavor, drive, root, tail)
    }

    /// Returns `pathlib.Path(text)` on this host.
    pub(crate) fn native(text: &str) -> Self {
        Self::new(Flavor::NATIVE, text)
    }

    /// Returns the path of these parts, formatted as `pathlib` formats them.
    fn from_parts(flavor: Flavor, drive: String, root: String, tail: Vec<String>) -> Self {
        let text = match flavor {
            Flavor::Posix => {
                let parts: Vec<&[u8]> = tail.iter().map(String::as_bytes).collect();
                from_utf8(&format_posix(root.as_bytes(), &parts))
            }
            Flavor::Windows => {
                let units = |text: &str| text.encode_utf16().collect::<Vec<u16>>();
                let parts: Vec<Vec<u16>> = tail.iter().map(|part| units(part)).collect();
                from_utf16(&format_windows(&units(&drive), &units(&root), &parts))
            }
        };
        Self {
            flavor,
            drive,
            root,
            tail,
            text,
        }
    }

    /// Returns the flavor of the path.
    pub(crate) fn flavor(&self) -> Flavor {
        self.flavor
    }

    /// Returns `str(path)`.
    pub(crate) fn as_str(&self) -> &str {
        &self.text
    }

    /// Returns `str(path)` as a host path.
    pub(crate) fn as_path(&self) -> &Path {
        Path::new(&self.text)
    }

    /// Returns `path.parts`: the drive and root together, then each component.
    pub(crate) fn parts(&self) -> Vec<String> {
        let anchor = format!("{}{}", self.drive, self.root);
        (!anchor.is_empty())
            .then_some(anchor)
            .into_iter()
            .chain(self.tail.iter().cloned())
            .collect()
    }

    /// Returns `path.name`: the last component, or an empty string.
    pub(crate) fn name(&self) -> &str {
        self.tail.last().map_or("", String::as_str)
    }

    /// Returns `path.parents`, from the parent to the anchor.
    pub(crate) fn parents(&self) -> impl Iterator<Item = Self> + '_ {
        (0..self.tail.len()).rev().map(|length| {
            Self::from_parts(
                self.flavor,
                self.drive.clone(),
                self.root.clone(),
                self.tail[..length].to_vec(),
            )
        })
    }

    /// Returns `path.is_absolute()`.
    pub(crate) fn is_absolute(&self) -> bool {
        self.flavor.is_absolute(&self.text)
    }

    /// Returns `path / segment`.
    #[must_use]
    pub(crate) fn join(&self, segment: &str) -> Self {
        Self::new(self.flavor, &self.flavor.join(&self.text, segment))
    }

    /// Returns `path.with_name(name)`, or `None` where it raises `ValueError`: for a name that is
    /// empty, `.`, or holds a separator, and for a path without components.
    pub(crate) fn with_name(&self, name: &str) -> Option<Self> {
        if name.is_empty() || name == "." || name.chars().any(|c| self.flavor.is_separator(c)) {
            return None;
        }
        let (_, rest) = self.tail.split_last()?;
        let mut tail = rest.to_vec();
        tail.push(name.to_owned());
        Some(Self::from_parts(
            self.flavor,
            self.drive.clone(),
            self.root.clone(),
            tail,
        ))
    }

    /// Returns the string by which `pathlib` compares the path: `str(path)`, in lowercase on
    /// Windows.
    fn case_normalized(&self) -> std::borrow::Cow<'_, str> {
        match self.flavor {
            Flavor::Posix => self.text.as_str().into(),
            Flavor::Windows => self.text.to_lowercase().into(),
        }
    }
}

impl PartialEq for PurePath {
    fn eq(&self, other: &Self) -> bool {
        self.flavor == other.flavor && self.case_normalized() == other.case_normalized()
    }
}

impl Eq for PurePath {}

impl fmt::Display for PurePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.text)
    }
}

impl fmt::Debug for PurePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let class = match self.flavor {
            Flavor::Posix => "PurePosixPath",
            Flavor::Windows => "PureWindowsPath",
        };
        write!(formatter, "{class}({:?})", self.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn posix(path: &str) -> String {
        String::from_utf8(posix_str(path.as_bytes())).unwrap()
    }

    fn windows(path: &str) -> String {
        String::from_utf16(&windows_str(&path.encode_utf16().collect::<Vec<_>>())).unwrap()
    }

    fn pure_posix(text: &str) -> PurePath {
        PurePath::new(Flavor::Posix, text)
    }

    fn pure_windows(text: &str) -> PurePath {
        PurePath::new(Flavor::Windows, text)
    }

    #[test]
    fn paths_normalize_as_pathlib_does() {
        // Expected values from Python 3.14's `PurePosixPath` and `PureWindowsPath` on Windows.
        for (path, expected_posix, expected_windows) in [
            ("", ".", "."),
            ("./", ".", "."),
            ("a//b", "a/b", "a\\b"),
            ("a/./b/.", "a/b", "a\\b"),
            ("../a", "../a", "..\\a"),
            ("/", "/", "\\"),
            ("//", "//", "\\\\"),
            ("///", "/", "\\\\\\"),
            ("////a", "/a", "\\\\\\\\a"),
            ("//a/b", "//a/b", "\\\\a\\b\\"),
            ("c:/a", "c:/a", "c:\\a"),
            ("c:\\a\\", "c:\\a\\", "c:\\a"),
            ("C:/", "C:", "C:\\"),
            (
                "\\\\server\\share",
                "\\\\server\\share",
                "\\\\server\\share\\",
            ),
            ("//?/UNC/a/b", "//?/UNC/a/b", "\\\\?\\UNC\\a\\b\\"),
            (
                "\\\\?\\unc\\server\\share",
                "\\\\?\\unc\\server\\share",
                "\\\\?\\unc\\server\\share\\",
            ),
            ("\\\\.\\device\\x", "\\\\.\\device\\x", "\\\\.\\device\\x"),
            ("\\\\\\share", "\\\\\\share", "\\\\\\share"),
            ("./c:foo", "c:foo", ".\\c:foo"),
            ("é:x", "é:x", "é:x"),
            (".\\.\\", ".\\.\\", "."),
            ("a /b", "a /b", "a \\b"),
            // The paths that the retired egress policy fixture recorded.
            (".", ".", "."),
            ("a", "a", "a"),
            ("a/", "a", "a"),
            ("./a", "a", "a"),
            ("a/../b", "a/../b", "a\\..\\b"),
            ("///a/b", "/a/b", "\\\\\\a\\b"),
            ("a\\b", "a\\b", "a\\b"),
            ("c:", "c:", "c:"),
            ("c:a", "c:a", "c:a"),
            ("/a", "/a", "\\a"),
            ("\\a", "\\a", "\\a"),
            ("\\\\server", "\\\\server", "\\\\server"),
            (
                "\\\\server\\share\\x",
                "\\\\server\\share\\x",
                "\\\\server\\share\\x",
            ),
            (
                "//server/share/x/",
                "//server/share/x",
                "\\\\server\\share\\x",
            ),
            ("\\\\?\\c:\\x", "\\\\?\\c:\\x", "\\\\?\\c:\\x"),
            (
                "\\\\?\\UNC\\server\\share\\x",
                "\\\\?\\UNC\\server\\share\\x",
                "\\\\?\\UNC\\server\\share\\x",
            ),
            ("\\\\?.\\x\\y", "\\\\?.\\x\\y", "\\\\?.\\x\\y"),
            ("c:foo/./bar", "c:foo/bar", "c:foo\\bar"),
            ("1:x", "1:x", "1:x"),
            ("::x", "::x", "::x"),
            ("a:b:c", "a:b:c", "a:b:c"),
            ("//?/UNC/a", "//?/UNC/a", "\\\\?\\UNC\\a"),
            ("//?/UNC/a/b/c", "//?/UNC/a/b/c", "\\\\?\\UNC\\a\\b\\c"),
            ("\\\\?\\", "\\\\?\\", "\\\\?\\"),
            ("\\\\.\\", "\\\\.\\", "\\\\.\\"),
            ("\\\\?\\x", "\\\\?\\x", "\\\\?\\x"),
            ("a/.", "a", "a"),
            ("x\\.\\y", "x\\.\\y", "x\\y"),
            (
                "\\\\a\\b\\c\\d\\e",
                "\\\\a\\b\\c\\d\\e",
                "\\\\a\\b\\c\\d\\e",
            ),
            (" a ", " a ", " a "),
            ("dir/é.json", "dir/é.json", "dir\\é.json"),
            ("policy.json", "policy.json", "policy.json"),
            // On Windows, a character beyond the Basic Multilingual Plane is two code units, so
            // the colon after it makes no drive, and a NUL before a colon makes one.
            ("\u{1F600}:x", "\u{1F600}:x", "\u{1F600}:x"),
            ("./\u{1F600}:x", "\u{1F600}:x", "\u{1F600}:x"),
            ("./\0:x", "\0:x", ".\\\0:x"),
        ] {
            assert_eq!(posix(path), expected_posix, "{path:?}");
            assert_eq!(windows(path), expected_windows, "{path:?}");
            assert_eq!(pure_posix(path).as_str(), expected_posix, "{path:?}");
            assert_eq!(pure_windows(path).as_str(), expected_windows, "{path:?}");
        }
    }

    #[test]
    fn posix_paths_hold_their_parts() {
        assert!(pure_posix("//a").is_absolute());
        assert!(!pure_posix("a/b").is_absolute());
        assert_eq!(pure_posix("/a/b").parts(), ["/", "a", "b"]);
        assert_eq!(pure_posix("a/b").name(), "b");
        assert_eq!(
            pure_posix("/a/b/c")
                .parents()
                .map(|p| p.text)
                .collect::<Vec<_>>(),
            ["/a/b", "/a", "/"]
        );
        assert_eq!(pure_posix("/a/b").join("c").as_str(), "/a/b/c");
        assert_eq!(pure_posix("/a/b").join("/c").as_str(), "/c");
        assert_ne!(pure_posix("/A"), pure_posix("/a"));
        assert_eq!(
            pure_posix("/a/b.erofs")
                .with_name("b.erofs.manifest.json")
                .unwrap()
                .as_str(),
            "/a/b.erofs.manifest.json"
        );
        assert!(pure_posix("/").with_name("x").is_none());
    }

    #[test]
    fn windows_paths_hold_their_parts() {
        assert!(pure_windows("C:\\a").is_absolute());
        assert!(pure_windows("//server/share").is_absolute());
        assert!(!pure_windows("\\a").is_absolute());
        assert!(!pure_windows("C:a").is_absolute());
        // `ntpath.isabs` tests code points, even where `splitroot` finds no drive.
        assert!(pure_windows("\u{1F600}:\\x").is_absolute());
        assert_eq!(pure_windows("\u{1F600}:\\x").parts(), ["\u{1F600}:", "x"]);
        assert_eq!(pure_windows("C:\\A"), pure_windows("c:/a"));
        assert_eq!(pure_windows("C:\\a\\b").parts(), ["C:\\", "a", "b"]);
        assert_eq!(
            pure_windows("C:\\cwd").join("a/b").as_str(),
            "C:\\cwd\\a\\b"
        );
        assert_eq!(pure_windows("C:\\cwd").join("C:x").as_str(), "C:\\cwd\\x");
        assert_eq!(pure_windows("C:\\cwd").join("D:x").as_str(), "D:x");
        assert_eq!(pure_windows("C:\\cwd").join("\\x").as_str(), "C:\\x");
        assert_eq!(
            Flavor::Windows.split_root("//server/share/x"),
            ("//server/share", "/", "x")
        );
        assert_eq!(
            Flavor::Windows.split_root("\u{1F600}:x"),
            ("", "", "\u{1F600}:x")
        );
        assert_eq!(Flavor::Windows.split_root("é:\\x"), ("é:", "\\", "x"));
    }

    #[test]
    fn windows_normpath_follows_cpython() {
        for (text, expected) in [
            ("", "."),
            (".", "."),
            ("a", "a"),
            ("a\\", "a"),
            ("A//B", "A\\B"),
            ("A/./B", "A\\B"),
            ("A/foo/../B", "A\\B"),
            ("C:\\", "C:\\"),
            ("C:", "C:"),
            ("C:a\\..\\..", "C:.."),
            ("C:\\..\\a", "C:\\a"),
            ("..\\..\\a", "..\\..\\a"),
            ("a\\..\\..\\b", "..\\b"),
            ("\\\\server\\share\\", "\\\\server\\share\\"),
            ("\\\\server\\share\\a\\..", "\\\\server\\share\\"),
            (".\\a\\.\\b\\", "a\\b"),
            ("\\", "\\"),
            ("\u{1F600}:x\\..\\y", "y"),
            ("\0:x\\..\\y", "\0:y"),
        ] {
            assert_eq!(windows_normpath(text), expected, "{text:?}");
        }
    }

    #[test]
    fn split_returns_the_head_and_the_last_component() {
        assert_eq!(
            Flavor::Windows.split("C:\\a\\b"),
            ("C:\\a".to_owned(), "b".to_owned())
        );
        assert_eq!(
            Flavor::Windows.split("C:\\a"),
            ("C:\\".to_owned(), "a".to_owned())
        );
        assert_eq!(
            Flavor::Windows.split("C:\\"),
            ("C:\\".to_owned(), String::new())
        );
        assert_eq!(
            Flavor::Posix.split("/a/b"),
            ("/a".to_owned(), "b".to_owned())
        );
        assert_eq!(Flavor::Posix.split("/a"), ("/".to_owned(), "a".to_owned()));
        assert_eq!(
            Flavor::Posix.split("//a"),
            ("//".to_owned(), "a".to_owned())
        );
    }
}
