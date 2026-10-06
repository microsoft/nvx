//! Operating-system primitives used by the OpenVMM backend.
//!
//! Each platform module provides the same functions. Linux and Windows are supported hosts;
//! other platforms compile but report the backend as unavailable.

use std::io;
use std::time::Duration;

#[cfg(feature = "nvxhost")]
use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(any(target_os = "linux", windows)))]
mod other;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
pub(crate) use linux::*;
#[cfg(not(any(target_os = "linux", windows)))]
pub(crate) use other::*;
#[cfg(windows)]
pub(crate) use windows::*;

/// Byte stream to an OpenVMM control endpoint.
pub(crate) trait Transport: Send {
    /// Reads at least one byte, waiting at most `timeout` (`None` waits indefinitely).
    ///
    /// Returns `Ok(0)` at end of stream and an [`io::ErrorKind::TimedOut`] error on timeout.
    fn read(&mut self, buffer: &mut [u8], timeout: Option<Duration>) -> io::Result<usize>;

    /// Writes all of `data`, waiting at most `timeout` for each write to complete.
    fn write_all(&mut self, data: &[u8], timeout: Option<Duration>) -> io::Result<()>;
}

/// Identity of a regular file and of its last change, compared instead of hashing the file again.
///
/// Replacing the file or writing its data changes the seal. A writer that restores the last-write
/// time afterwards can preserve it on Windows, and a write within the file system's timestamp
/// granularity can preserve it on Linux, so a seal detects accidental change rather than
/// deliberate tampering. On Linux, any metadata change also changes the seal, because the seal
/// includes the change time, which writers cannot restore. Windows seals omit the change time:
/// writers can set it, and the system updates it when security components cache a file's hash in
/// its extended attributes, which would fail unchanged files closed.
#[cfg(feature = "nvxhost")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct FileSeal {
    /// Platform scheme of the remaining fields.
    pub(crate) scheme: String,
    /// Volume serial number or device number.
    pub(crate) volume: u64,
    /// File ID or inode number, in hexadecimal.
    pub(crate) file: String,
    /// Length in bytes.
    pub(crate) length: u64,
    /// Last data modification, in the scheme's time unit.
    pub(crate) modified: i64,
    /// Last data or metadata change, in the scheme's time unit, where the scheme seals it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) changed: Option<i64>,
}
