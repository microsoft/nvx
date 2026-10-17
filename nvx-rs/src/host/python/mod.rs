//! Python semantics that the faithful ports of `scripts/nvx.py` tools need.
//!
//! Commands that `nvx.py` delegates to `nvx-rs` keep the behavior of the Python code they
//! replace, which depends on how Python reads JSON, holds strings, and prints paths:
//!
//! - [`text`]: Python strings, which may hold unpaired surrogates, and how Python prints them.
//! - [`json`]: a JSON reader that accepts what `json.loads` accepts, with the equivalents of its
//!   `object_pairs_hook` and `parse_int` hooks.
//! - [`path`]: `str(pathlib.Path(...))`, which Python messages show, and the path that Python
//!   opens for it.

pub(crate) mod json;
pub(crate) mod path;
pub(crate) mod text;
