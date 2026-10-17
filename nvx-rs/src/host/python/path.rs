//! Paths as Python's `pathlib` prints and opens them.
//!
//! Python messages name files by `str(path)` for a `pathlib.Path`, which normalizes the path it
//! was given: it drops empty and `.` components, and on Windows turns `/` into `\` and gives UNC
//! shares a root. Python also opens that string, not the one it was given, so `config.json/`
//! opens `config.json`. [`python_path`] reproduces both for this host's flavor of paths.

use std::path::{Path, PathBuf};

/// A path as a `pathlib.Path` of this host holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PythonPath {
    /// `str(path)` as Python prints it to a UTF-8 stream, which escapes surrogates as `\udXXX`.
    pub(crate) display: String,
    /// The path that Python opens: `os.fspath(path)`.
    pub(crate) native: PathBuf,
}

/// Returns `path` as Python's `pathlib.Path(path)` prints and opens it.
///
/// On Unix, Python decodes bytes that are not UTF-8 into surrogates and prints each as `\udcXX`;
/// on Windows, unpaired surrogates print as `\udXXX`.
pub(crate) fn python_path(path: &Path) -> PythonPath {
    #[cfg(windows)]
    {
        use std::ffi::OsString;
        use std::os::windows::ffi::{OsStrExt, OsStringExt};

        let units: Vec<u16> = path.as_os_str().encode_wide().collect();
        let normalized = windows_str(&units);
        PythonPath {
            display: render_utf16(&normalized),
            native: PathBuf::from(OsString::from_wide(&normalized)),
        }
    }
    #[cfg(unix)]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let normalized = posix_str(path.as_os_str().as_bytes());
        PythonPath {
            display: render_surrogateescape(&normalized),
            native: PathBuf::from(OsStr::from_bytes(&normalized)),
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let normalized = posix_str(path.to_string_lossy().as_bytes());
        let display = String::from_utf8_lossy(&normalized).into_owned();
        PythonPath {
            native: PathBuf::from(&display),
            display,
        }
    }
}

/// Returns `str(PurePosixPath(path))` for a path given as bytes, which Python sees as the
/// characters that they decode to.
///
/// One leading `/`, or three or more, are a root of `/`, and exactly two are a root of `//`;
/// empty and `.` components are dropped, and an empty result is `.`.
#[cfg(any(not(windows), test))]
pub(crate) fn posix_str(path: &[u8]) -> Vec<u8> {
    let (root, rest): (&[u8], &[u8]) = match path {
        [b'/', b'/', third, ..] if *third != b'/' => (b"//", &path[2..]),
        [b'/', b'/'] => (b"//", &path[2..]),
        [b'/', ..] => (b"/", &path[1..]),
        _ => (b"", path),
    };
    let mut result = root.to_vec();
    let mut parts = rest
        .split(|&byte| byte == b'/')
        .filter(|part| !part.is_empty() && *part != b".");
    if let Some(first) = parts.next() {
        result.extend_from_slice(first);
        for part in parts {
            result.push(b'/');
            result.extend_from_slice(part);
        }
    }
    if result.is_empty() {
        result.push(b'.');
    }
    result
}

/// The separator of Windows paths, `\`.
#[cfg(any(windows, test))]
const SEPARATOR: u16 = b'\\' as u16;

/// Returns `str(PureWindowsPath(path))` on Windows for a path given as UTF-16 code units, as
/// Python 3.14 computes it with `nt._path_splitroot_ex`, which indexes UTF-16 code units.
///
/// `/` becomes `\`; a drive is a letter or other code unit and a colon, a UNC share
/// (`\\server\share`, `\\?\UNC\server\share`), or a device (`\\.\device`, `\\?\device`); UNC
/// shares gain a root; empty and `.` components are dropped; a relative path whose first
/// component looks like a drive gains a `.\` prefix; and an empty result is `.`.
#[cfg(any(windows, test))]
pub(crate) fn windows_str(path: &[u16]) -> Vec<u16> {
    let path: Vec<u16> = path
        .iter()
        .map(|&unit| {
            if unit == u16::from(b'/') {
                SEPARATOR
            } else {
                unit
            }
        })
        .collect();
    let (drive_length, root_length) = windows_skip_root(&path);
    let drive = &path[..drive_length];
    let mut root = &path[drive_length..drive_length + root_length];
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
            root = &[SEPARATOR];
        }
    }
    let tail: Vec<&[u16]> = path[drive_length + root_length..]
        .split(|&unit| unit == SEPARATOR)
        .filter(|part| !part.is_empty() && *part != [u16::from(b'.')])
        .collect();
    let mut result = Vec::with_capacity(path.len() + 2);
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

/// Returns the lengths of the drive and the root of a Windows path whose separators are `\`, as
/// CPython's `_Py_skiproot` computes them.
#[cfg(any(windows, test))]
fn windows_skip_root(path: &[u16]) -> (usize, usize) {
    let unit = |index: usize| path.get(index).copied();
    let separator = |index: usize| unit(index) == Some(SEPARATOR);
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

/// Returns UTF-16 code units as Python prints them to a UTF-8 stream: unpaired surrogates as
/// `\udXXX`.
#[cfg(any(windows, test))]
fn render_utf16(units: &[u16]) -> String {
    char::decode_utf16(units.iter().copied())
        .map(|unit| match unit {
            Ok(character) => character.to_string(),
            Err(error) => format!("\\u{:04x}", error.unpaired_surrogate()),
        })
        .collect()
}

/// Returns bytes as Python prints the string that it decodes them to with the `surrogateescape`
/// error handler: valid UTF-8 as is, and each other byte as `\udcXX`.
#[cfg(any(unix, test))]
fn render_surrogateescape(bytes: &[u8]) -> String {
    let mut rendered = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        rendered.push_str(chunk.valid());
        for byte in chunk.invalid() {
            rendered.push_str(&format!("\\udc{byte:02x}"));
        }
    }
    rendered
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
        ] {
            assert_eq!(posix(path), expected_posix, "{path:?}");
            assert_eq!(windows(path), expected_windows, "{path:?}");
        }
    }

    #[test]
    fn unprintable_paths_render_as_python_prints_them() {
        assert_eq!(
            render_surrogateescape(b"a\xffb\xe2\x82"),
            "a\\udcffb\\udce2\\udc82"
        );
        assert_eq!(render_surrogateescape("é".as_bytes()), "é");
        assert_eq!(render_utf16(&[0x61, 0xD800, 0x62]), "a\\ud800b");
        assert_eq!(render_utf16(&[0xD83D, 0xDE00]), "\u{1F600}");
    }

    #[test]
    fn hosts_open_the_normalized_path() {
        let path = python_path(Path::new("a//b/./policy.json/"));
        if cfg!(windows) {
            assert_eq!(path.display, "a\\b\\policy.json");
        } else {
            assert_eq!(path.display, "a/b/policy.json");
        }
        assert_eq!(path.native, PathBuf::from(&path.display));
    }
}
