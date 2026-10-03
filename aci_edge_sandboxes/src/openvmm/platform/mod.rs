//! Operating-system primitives used by the OpenVMM backend.
//!
//! Each platform module provides the same functions. Linux and Windows are supported hosts;
//! other platforms compile but report the backend as unavailable.

use std::io;
use std::time::Duration;

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
