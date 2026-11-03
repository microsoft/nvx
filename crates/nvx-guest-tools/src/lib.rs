//! The helper programs of NVX's Linux guests. It holds no code yet: the
//! programs will move here from `nvx`. They run only in Linux, so the crate is
//! empty on Windows and every other target.
// On other targets, the crate keeps only the attributes above this one: its documentation.
#![cfg(target_os = "linux")]
