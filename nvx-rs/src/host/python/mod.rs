//! Python semantics that the faithful ports of `scripts/nvx.py` tools need.
//!
//! Commands that `nvx.py` delegates to `nvx-rs` keep the behavior of the Python code they
//! replace, which depends on how Python reads JSON and integers, holds and prints strings and
//! paths, and queries the filesystem. These modules implement what the ports rely on, after
//! CPython 3.14:
//!
//! - [`text`]: Python strings, which may hold unpaired surrogates, how Python prints them, and
//!   the errors of its UTF-8 codec.
//! - [`builtins`]: `str.isspace`, `str.split()`, `str.strip()`, `repr` of a `str`, and `int`.
//! - [`json`]: a JSON reader that accepts what `json.loads` accepts, with the equivalents of its
//!   `object_pairs_hook` and `parse_int` hooks, and that fails with its messages.
//! - [`path`]: `str(pathlib.Path(...))`, which Python messages show, the path that Python opens
//!   for it, and the pure paths of `pathlib` with the `os.path` operations that they use.
//! - [`uuid`]: the UUIDs that `uuid.UUID` reads.
//! - [`fs`]: the filesystem queries of `os`, `os.path`, and `pathlib.Path`, among them
//!   `Path.resolve()` and the file identities of `os.stat`.
//!
//! Character properties come from the Unicode 16.0.0 database of Python 3.14, in [`unicode`],
//! which `scripts/nvx_rs_fixtures.py` generates. The same script records what Python does with
//! fixed and seeded random inputs in `tests/fixtures`, and the golden tests compare these
//! implementations, and the ports that use them, with those records. Python decodes
//! operating-system strings that are not valid Unicode into surrogates: [`path`] prints such
//! paths as Python does, and the filesystem queries refuse them.

pub(crate) mod builtins;
pub(crate) mod fs;
#[cfg(test)]
pub(crate) mod golden;
pub(crate) mod json;
pub(crate) mod path;
pub(crate) mod text;
pub(crate) mod unicode;
pub(crate) mod uuid;
#[cfg(windows)]
mod windows;

/// Returns the error for an operating-system string that is not valid Unicode.
pub(crate) fn not_unicode(what: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("{what} is not valid Unicode"),
    )
}
