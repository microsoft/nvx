//! NVX's init for its Linux guests: the Rust PID 1 of layered roots, and the
//! managed agent. It holds no code yet: they will move here from `nvx`. They
//! run only in Linux, so the crate is empty on Windows and every other target.
// On other targets, the crate keeps only the attributes above this one: its documentation.
#![cfg(target_os = "linux")]
