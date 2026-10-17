//! The filesystem queries of Python's `os`, `os.path`, and `pathlib.Path`, as CPython 3.14
//! performs them on Linux and Windows.
//!
//! - [`cwd`]: the current directory, as `Path.cwd()` gives it.
//! - [`is_file`], [`is_dir`], and [`is_symlink`]: `os.path.isfile`, `isdir`, and `islink`, which
//!   follow symbolic links, except `islink`, and are false on any error. On
//!   Windows, only `IO_REPARSE_TAG_SYMLINK` reparse points are links, not junctions.
//! - [`stat`]: the identity and owner that `os.stat` reports. On Windows, the identity is the
//!   64-bit volume serial number and the 128-bit file ID of `FILE_ID_INFO`, as Python 3.12 and
//!   later report them, which also tell apart the files of ReFS volumes.
//! - [`resolve`]: `Path.resolve()`, which follows `os.path.realpath`: it resolves symbolic links
//!   component by component on POSIX, keeping the components that do not exist, and asks Windows
//!   for the final path of the longest existing prefix, dropping the `\\?\` prefix again.
//! - [`os_error_message`]: the text of an `OSError`.
//!
//! Paths must be valid Unicode: a link target or working directory that is not is an error,
//! where Python would decode it into surrogates.

use std::io;
use std::path::Path;

use super::builtins::repr;
use super::not_unicode;
use super::path::PurePath;

/// The fields of `os.stat(path)` that NVX reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stat {
    /// `st_dev`: the device, or on Windows the volume serial number.
    pub(crate) device: u64,
    /// `st_ino`: the inode, or on Windows the file ID.
    pub(crate) inode: u128,
    /// `st_uid`: the owner, or zero on Windows.
    pub(crate) uid: u32,
    /// `st_gid`: the group, or zero on Windows.
    pub(crate) gid: u32,
}

impl Stat {
    /// Returns `(st_dev, st_ino)`, the identity of the file.
    pub(crate) fn identity(&self) -> (u64, u128) {
        (self.device, self.inode)
    }
}

/// Returns `Path.cwd()`.
pub(crate) fn cwd() -> io::Result<PurePath> {
    let directory = std::env::current_dir()?;
    let text = directory
        .to_str()
        .ok_or_else(|| not_unicode("the current directory"))?;
    Ok(PurePath::native(text))
}

/// Returns `os.path.isfile(path)`: whether `path` is a regular file, following links.
pub(crate) fn is_file(path: &Path) -> bool {
    imp::is_file(path)
}

/// Returns `os.path.isdir(path)`: whether `path` is a directory, following links.
pub(crate) fn is_dir(path: &Path) -> bool {
    imp::is_dir(path)
}

/// Returns `os.path.islink(path)`: whether `path` is a symbolic link.
pub(crate) fn is_symlink(path: &Path) -> bool {
    imp::is_symlink(path)
}

/// Returns `os.stat(path)`, following links.
pub(crate) fn stat(path: &Path) -> io::Result<Stat> {
    imp::stat(path)
}

/// Returns `path.resolve()`: the absolute path of `path`, with symbolic links resolved and `..`
/// components removed, as `os.path.realpath(path)` gives it.
pub(crate) fn resolve(path: &PurePath) -> io::Result<PurePath> {
    imp::realpath(path.as_str()).map(|resolved| PurePath::native(&resolved))
}

/// Returns `str(error)` for an `OSError` that an operation on `filename` raised: `[Errno N]
/// text: 'filename'` on POSIX, and `[WinError N] text: 'filename'` on Windows, with the text
/// that the system gives for the error.
pub(crate) fn os_error_message(error: &io::Error, filename: Option<&str>) -> String {
    let Some(code) = error.raw_os_error() else {
        return error.to_string();
    };
    // Rust appends ` (os error N)` to the system's text, and Python drops the trailing period
    // and whitespace of Windows messages.
    let text = error.to_string();
    let text = text
        .strip_suffix(&format!(" (os error {code})"))
        .unwrap_or(&text);
    let (label, text) = if cfg!(windows) {
        (
            "WinError",
            text.trim_end_matches(|c: char| c <= ' ' || c == '.'),
        )
    } else {
        ("Errno", text)
    };
    match filename {
        Some(filename) => format!("[{label} {code}] {text}: {}", repr(filename)),
        None => format!("[{label} {code}] {text}"),
    }
}

#[cfg(unix)]
/// The POSIX implementation, on the standard library.
mod imp {
    use std::fs;
    use std::io;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    use super::{Stat, not_unicode};

    /// See [`super::is_file`].
    pub(super) fn is_file(path: &Path) -> bool {
        fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
    }

    /// See [`super::is_dir`].
    pub(super) fn is_dir(path: &Path) -> bool {
        fs::metadata(path).is_ok_and(|metadata| metadata.is_dir())
    }

    /// See [`super::is_symlink`].
    pub(super) fn is_symlink(path: &Path) -> bool {
        fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
    }

    /// See [`super::stat`].
    pub(super) fn stat(path: &Path) -> io::Result<Stat> {
        let metadata = fs::metadata(path)?;
        Ok(Stat {
            device: metadata.dev(),
            inode: u128::from(metadata.ino()),
            uid: metadata.uid(),
            gid: metadata.gid(),
        })
    }

    /// Returns `posixpath.realpath(path)` in its non-strict mode: each component is resolved in
    /// turn, a symbolic link is replaced by its target, and a component that cannot be examined
    /// is kept as it is. A symbolic-link loop stops resolution at the link.
    pub(super) fn realpath(path: &str) -> io::Result<String> {
        // The unresolved components, last first. `None` marks the end of a link's target, and
        // is followed by the link's own path.
        let mut rest: Vec<Option<String>> = path
            .split('/')
            .rev()
            .map(|part| Some(part.to_owned()))
            .collect();
        let mut part_count = rest.len();
        let mut resolved = if path.starts_with('/') {
            "/".to_owned()
        } else {
            let directory = std::env::current_dir()?;
            directory
                .to_str()
                .ok_or_else(|| not_unicode("the current directory"))?
                .to_owned()
        };
        // Each link that has been seen, with its resolved target once known.
        let mut seen: std::collections::HashMap<String, Option<String>> =
            std::collections::HashMap::new();
        while part_count > 0 {
            let Some(name) = rest.pop() else {
                break;
            };
            let Some(name) = name else {
                // The target of a link is resolved.
                if let Some(Some(link)) = rest.pop() {
                    seen.insert(link, Some(resolved.clone()));
                }
                continue;
            };
            part_count -= 1;
            if name.is_empty() || name == "." {
                continue;
            }
            if name == ".." {
                let end = resolved.rfind('/').unwrap_or(0);
                resolved.truncate(end);
                if resolved.is_empty() {
                    resolved.push('/');
                }
                continue;
            }
            let candidate = if resolved == "/" {
                format!("/{name}")
            } else {
                format!("{resolved}/{name}")
            };
            if candidate.contains('\0') {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "embedded null byte",
                ));
            }
            let target = match fs::symlink_metadata(&candidate) {
                Ok(metadata) if !metadata.file_type().is_symlink() => {
                    resolved = candidate;
                    continue;
                }
                Ok(_) => match seen.get(&candidate) {
                    Some(Some(target)) => {
                        resolved = target.clone();
                        continue;
                    }
                    // A link whose resolution is in progress: a loop.
                    Some(None) => {
                        resolved = candidate;
                        continue;
                    }
                    None => fs::read_link(&candidate),
                },
                Err(error) => Err(error),
            };
            let Ok(target) = target else {
                // Errors are ignored: the component is kept as it is.
                resolved = candidate;
                continue;
            };
            let target = target
                .into_os_string()
                .into_string()
                .map_err(|_| not_unicode("a symbolic link's target"))?;
            if target.starts_with('/') {
                "/".clone_into(&mut resolved);
            }
            seen.insert(candidate.clone(), None);
            rest.push(Some(candidate));
            rest.push(None);
            let parts: Vec<&str> = target.split('/').collect();
            part_count += parts.len();
            rest.extend(parts.into_iter().rev().map(|part| Some(part.to_owned())));
        }
        Ok(resolved)
    }
}

#[cfg(windows)]
/// The Windows implementation, on the Win32 calls that CPython makes.
mod imp {
    use std::io;
    use std::path::Path;

    use super::super::path::{Flavor, windows_normpath};
    use super::super::windows::{self as win32, StatMode};
    use super::Stat;

    /// See [`super::is_file`].
    pub(super) fn is_file(path: &Path) -> bool {
        win32::stat(path, true).is_ok_and(|status| status.mode == StatMode::Regular)
    }

    /// See [`super::is_dir`].
    pub(super) fn is_dir(path: &Path) -> bool {
        win32::stat(path, true).is_ok_and(|status| status.mode == StatMode::Directory)
    }

    /// See [`super::is_symlink`].
    pub(super) fn is_symlink(path: &Path) -> bool {
        win32::stat(path, false).is_ok_and(|status| status.mode == StatMode::Link)
    }

    /// See [`super::stat`].
    pub(super) fn stat(path: &Path) -> io::Result<Stat> {
        let status = win32::stat(path, true)?;
        Ok(Stat {
            device: status.device,
            inode: status.inode,
            uid: 0,
            gid: 0,
        })
    }

    /// Returns `ntpath.normcase(path)`: lowercase, with `\` separators.
    fn normcase(path: &str) -> String {
        path.replace('/', "\\").to_lowercase()
    }

    /// Returns `ntpath.dirname(path)`.
    fn dirname(path: &str) -> String {
        Flavor::Windows.split(path).0
    }

    /// Returns whether `error` is one of the Win32 errors in `codes`.
    fn is_error(error: &io::Error, codes: &[i32]) -> bool {
        error
            .raw_os_error()
            .is_some_and(|code| codes.contains(&code))
    }

    /// Follows the links at `path` as `ntpath._readlink_deep` does, stopping at a path that is
    /// not a link, at a loop, or at an error that ends resolution.
    fn readlink_deep(path: &str) -> io::Result<String> {
        const STOP: [i32; 12] = [1, 2, 3, 5, 21, 32, 50, 67, 87, 4390, 4392, 4393];
        let mut path = path.to_owned();
        let mut seen = std::collections::HashSet::new();
        while seen.insert(normcase(&path)) {
            let old = path.clone();
            match win32::read_link(Path::new(&path)) {
                Ok(Some(target)) => {
                    path = target;
                    if !Flavor::Windows.is_absolute(&path) {
                        // A relative target resolves against the link's own directory, but
                        // only a symbolic link has one.
                        if !super::is_symlink(Path::new(&old)) {
                            path = old;
                            break;
                        }
                        path = windows_normpath(&Flavor::Windows.join(&dirname(&old), &path));
                    }
                }
                // A reparse point that is not a link.
                Ok(None) => break,
                Err(error) if is_error(&error, &STOP) => break,
                Err(error) => return Err(error),
            }
        }
        Ok(path)
    }

    /// Resolves as much of `path` as exists and appends the rest, as
    /// `ntpath._getfinalpathname_nonstrict` does.
    fn final_path_name_nonstrict(path: &str) -> io::Result<String> {
        const STOP: [i32; 16] = [
            1, 2, 3, 5, 21, 32, 50, 53, 65, 67, 87, 123, 161, 1005, 1920, 1921,
        ];
        const BY_NAME: [i32; 7] = [1, 5, 32, 50, 87, 1920, 1921];
        let mut path = path.to_owned();
        let mut tail = String::new();
        while !path.is_empty() {
            let error = match win32::final_path_name(Path::new(&path)) {
                Ok(resolved) => {
                    return Ok(if tail.is_empty() {
                        resolved
                    } else {
                        Flavor::Windows.join(&resolved, &tail)
                    });
                }
                Err(error) if is_error(&error, &STOP) => error,
                Err(error) => return Err(error),
            };
            // Windows could not resolve the path: follow its links ourselves.
            match readlink_deep(&path) {
                Ok(target) if target != path => {
                    return Ok(if tail.is_empty() {
                        target
                    } else {
                        Flavor::Windows.join(&target, &tail)
                    });
                }
                _ => {}
            }
            let (head, name) = if is_error(&error, &BY_NAME) {
                // Read the real name of the file without opening it.
                match win32::find_first_file_name(Path::new(&path)) {
                    Ok(name) => (Flavor::Windows.split(&path).0, name),
                    Err(_) => Flavor::Windows.split(&path),
                }
            } else {
                Flavor::Windows.split(&path)
            };
            path = head;
            if !path.is_empty() && name.is_empty() {
                return Ok(format!("{path}{tail}"));
            }
            tail = if tail.is_empty() {
                name
            } else {
                Flavor::Windows.join(&name, &tail)
            };
        }
        Ok(tail)
    }

    /// Returns `ntpath.realpath(path)` in its non-strict mode.
    pub(super) fn realpath(path: &str) -> io::Result<String> {
        const PREFIX: &str = "\\\\?\\";
        const UNC_PREFIX: &str = "\\\\?\\UNC\\";
        let mut path = windows_normpath(path);
        if normcase(&path) == "nul" {
            return Ok("\\\\.\\NUL".to_owned());
        }
        let had_prefix = path.starts_with(PREFIX);
        if !had_prefix && !Flavor::Windows.is_absolute(&path) {
            let directory = std::env::current_dir()?;
            let directory = directory
                .to_str()
                .ok_or_else(|| super::not_unicode("the current directory"))?;
            path = Flavor::Windows.join(directory, &path);
        }
        if path.contains('\0') {
            // Python cannot pass the path to Windows and returns it as it is.
            return Ok(windows_normpath(&path));
        }
        let initial_error = match win32::final_path_name(Path::new(&path)) {
            Ok(resolved) => {
                path = resolved;
                None
            }
            Err(error) => {
                let code = error.raw_os_error();
                path = final_path_name_nonstrict(&path)?;
                code
            }
        };
        if !had_prefix && path.starts_with(PREFIX) {
            let stripped = match path.strip_prefix(UNC_PREFIX) {
                Some(rest) => format!("\\\\{rest}"),
                None => path[PREFIX.len()..].to_owned(),
            };
            // Drop the prefix if the path without it resolves to the same path, or if neither
            // exists for the same reason.
            match win32::final_path_name(Path::new(&stripped)) {
                Ok(resolved) if resolved == path => path = stripped,
                Ok(_) => {}
                Err(error) if error.raw_os_error() == initial_error.or(Some(0)) => {
                    path = stripped;
                }
                Err(_) => {}
            }
        }
        Ok(path)
    }
}

#[cfg(not(any(unix, windows)))]
/// Hosts other than POSIX and Windows have no filesystem queries.
mod imp {
    use std::io;
    use std::path::Path;

    use super::Stat;

    fn unsupported() -> io::Error {
        io::Error::new(io::ErrorKind::Unsupported, "unsupported host")
    }

    pub(super) fn is_file(_path: &Path) -> bool {
        false
    }

    pub(super) fn is_dir(_path: &Path) -> bool {
        false
    }

    pub(super) fn is_symlink(_path: &Path) -> bool {
        false
    }

    pub(super) fn stat(_path: &Path) -> io::Result<Stat> {
        Err(unsupported())
    }

    pub(super) fn realpath(_path: &str) -> io::Result<String> {
        Err(unsupported())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn queries_follow_links_except_islink() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let file = root.join("file");
        let folder = root.join("folder");
        fs::write(&file, b"x").unwrap();
        fs::create_dir(&folder).unwrap();
        assert!(is_file(&file) && !is_dir(&file) && !is_symlink(&file));
        assert!(is_dir(&folder) && !is_file(&folder) && !is_symlink(&folder));
        assert!(!is_file(&root.join("missing")) && !is_dir(&root.join("missing")));
        assert!(!is_file(Path::new("nul\0byte")));
        let link = root.join("link");
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&file, &link);
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(&file, &link);
        if linked.is_ok() {
            assert!(is_symlink(&link) && is_file(&link));
            assert_eq!(
                stat(&link).unwrap().identity(),
                stat(&file).unwrap().identity()
            );
        }
        assert_ne!(
            stat(&file).unwrap().identity(),
            stat(&folder).unwrap().identity()
        );
    }

    #[test]
    fn hard_links_share_an_identity() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("file");
        let link = directory.path().join("hard");
        fs::write(&file, b"x").unwrap();
        fs::hard_link(&file, &link).unwrap();
        assert_eq!(
            stat(&file).unwrap().identity(),
            stat(&link).unwrap().identity()
        );
    }

    #[test]
    fn resolve_keeps_missing_components_and_follows_links() {
        let directory = tempfile::tempdir().unwrap();
        let root = resolve(&PurePath::native(directory.path().to_str().unwrap())).unwrap();
        assert!(root.is_absolute());
        let target = root.join("target");
        fs::create_dir(target.as_str()).unwrap();
        assert_eq!(resolve(&root.join("target")).unwrap(), target);
        assert_eq!(
            resolve(&root.join("missing/deeper")).unwrap(),
            root.join("missing").join("deeper")
        );
        assert_eq!(resolve(&root.join("target/../target")).unwrap(), target);
        let link = root.join("link");
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(target.as_str(), link.as_str());
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_dir(target.as_str(), link.as_str());
        if linked.is_ok() {
            assert_eq!(resolve(&link).unwrap(), target);
            assert_eq!(resolve(&link.join("child")).unwrap(), target.join("child"));
        }
    }

    #[test]
    fn os_errors_read_as_python_formats_them() {
        let error = fs::metadata("definitely/missing/path").unwrap_err();
        let message = os_error_message(&error, Some("missing"));
        let label = if cfg!(windows) {
            "[WinError 3]"
        } else {
            "[Errno 2]"
        };
        assert!(message.starts_with(label), "{message}");
        assert!(message.ends_with(": 'missing'"), "{message}");
        assert!(!message.contains("os error"), "{message}");
    }
}
