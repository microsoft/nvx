//! The OS layer that NVX's crates share, to which `nvx`'s OS modules move:
//!
//! - [`error`]: the error of NVX's operations, its classification, and its
//!   stable codes, which `nvx` re-exports.

pub mod error;
