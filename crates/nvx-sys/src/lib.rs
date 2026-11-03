//! The OS layer that NVX's crates share, to which `nvx`'s OS modules move:
//!
//! - [`error`]: the error of NVX's operations, its classification, and its
//!   stable codes, which `nvx` re-exports.
//! - `guest`, on Linux: the OS layer of NVX's guest programs, whose descriptor and socket
//!   primitives `nvx`'s guest modules use.

pub mod error;
#[cfg(target_os = "linux")]
pub mod guest;
