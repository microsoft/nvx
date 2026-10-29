//! Operating-system primitives of NVX hosts.
//!
//! Each platform module provides the same functions. Linux and Windows are supported hosts;
//! other platforms compile but report NVX as unavailable.
//!
//! The crate-private `ProcessHandle` of each platform holds a process that `open_process` found
//! with its recorded identity, or a child that the caller has not reaped yet: a pidfd on Linux,
//! when the host provides them, and a process handle on Windows. Waits and termination go through
//! it, so they reach that process and never another that reuses its ID.

use std::io;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::hypervisor::Hypervisor;

pub(crate) mod inherit;
#[cfg(target_os = "linux")]
/// Linux implementations of host process, socket, and hypervisor primitives.
mod linux;
#[cfg(not(any(target_os = "linux", windows)))]
mod other;
#[cfg(windows)]
/// Windows implementations of host process, pipe, and hypervisor primitives.
#[allow(unsafe_code)]
mod windows;

#[cfg(target_os = "linux")]
use linux as imp;
#[cfg(not(any(target_os = "linux", windows)))]
use other as imp;
#[cfg(windows)]
use windows as imp;

pub(crate) use imp::*;
pub use imp::{SUPPORTED, create_private_dir, file_identity, process_start_time};

/// Checks that `hypervisor` is present and accessible on this host: its device on Linux, and the
/// Windows Hypervisor Platform on Windows.
///
/// Fails with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) otherwise.
pub fn probe_hypervisor(hypervisor: Hypervisor) -> Result<()> {
    imp::probe_hypervisor(hypervisor).map_err(Error::unavailable)
}

/// Byte stream to an OpenVMM control endpoint.
pub trait Transport: Send {
    /// Reads at least one byte, waiting at most `timeout` (`None` waits indefinitely).
    ///
    /// Returns `Ok(0)` at end of stream and an [`io::ErrorKind::TimedOut`] error on timeout.
    fn read(&mut self, buffer: &mut [u8], timeout: Option<Duration>) -> io::Result<usize>;

    /// Writes all of `data` within `timeout` (`None` waits indefinitely), and fails with an
    /// [`io::ErrorKind::TimedOut`] error if it has not by then, however much of it the endpoint
    /// reads meanwhile.
    fn write_all(&mut self, data: &[u8], timeout: Option<Duration>) -> io::Result<()>;
}

/// Returns the time left until `deadline`, `None` without one, or an [`io::ErrorKind::TimedOut`]
/// error once it has passed.
#[cfg(any(target_os = "linux", windows))]
fn time_left(deadline: Option<std::time::Instant>) -> io::Result<Option<Duration>> {
    match deadline {
        None => Ok(None),
        Some(deadline) => deadline
            .checked_duration_since(std::time::Instant::now())
            .filter(|left| !left.is_zero())
            .map(Some)
            .ok_or_else(|| io::ErrorKind::TimedOut.into()),
    }
}

/// Returns the outcome of ending a held process, given the `result` of signaling it: a failure
/// stands unless `exited` shows that the process has exited, which needs no ending.
///
/// Hosts refuse to signal some processes that exited: Windows those that this user may not
/// terminate or that already ended, and Linux another user's process until its parent reaps it.
#[cfg(any(target_os = "linux", windows))]
pub(crate) fn unless_exited(
    result: io::Result<()>,
    exited: impl FnOnce() -> io::Result<bool>,
) -> io::Result<()> {
    let Err(error) = result else {
        return Ok(());
    };
    if exited()? { Ok(()) } else { Err(error) }
}

/// Parses exactly 32 hexadecimal digits, in either case, into the 16 bytes that they spell.
pub(crate) fn parse_hex_id(digits: &str) -> Option<[u8; 16]> {
    let digits = digits.as_bytes();
    if digits.len() != 32 {
        return None;
    }
    let mut id = [0; 16];
    for (byte, pair) in id.iter_mut().zip(digits.chunks_exact(2)) {
        let nibble = |digit: u8| char::from(digit).to_digit(16);
        *byte = u8::try_from(nibble(pair[0])? * 16 + nibble(pair[1])?).ok()?;
    }
    Some(id)
}

/// Parses 64 hexadecimal digits, in either case, into the 32 bytes that they spell.
pub(crate) fn parse_hex32(text: &str) -> Option<[u8; 32]> {
    let digits = text.as_bytes();
    if digits.len() != 64 {
        return None;
    }
    let mut bytes = [0; 32];
    for (byte, pair) in bytes.iter_mut().zip(digits.chunks_exact(2)) {
        let nibble = |digit: u8| char::from(digit).to_digit(16);
        *byte = u8::try_from(nibble(pair[0])? * 16 + nibble(pair[1])?).ok()?;
    }
    Some(bytes)
}

/// Parses the identity of a host as OpenVMM reads it: 32 hexadecimal digits, in either case,
/// between optional braces and surrounding whitespace, ignoring dashes, as in `/etc/machine-id`
/// and the Windows machine GUID.
#[cfg(any(target_os = "linux", windows))]
fn parse_host_id(text: &str) -> Option<[u8; 16]> {
    let digits: String = text
        .trim()
        .trim_start_matches('{')
        .trim_end_matches('}')
        .chars()
        .filter(|&character| character != '-')
        .collect();
    parse_hex_id(&digits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn host_identities_are_read_as_openvmm_reads_them() {
        let id = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
            0xcd, 0xef,
        ];
        for text in [
            "0123456789abcdef0123456789ABCDEF\n",
            "01234567-89ab-cdef-0123-456789abcdef",
            "{01234567-89AB-CDEF-0123-456789ABCDEF}",
            " 0123456789abcdef0123456789abcdef ",
        ] {
            assert_eq!(parse_host_id(text), Some(id), "{text:?}");
        }
        for text in [
            "0123",
            "0123456789abcdef0123456789abcdeg",
            "0123456789abcdef0123456789abcdef00",
            "",
        ] {
            assert_eq!(parse_host_id(text), None, "{text:?}");
        }
    }

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn the_hosts_identity_and_monotonic_time_are_read() {
        let first = monotonic_ns().unwrap();
        assert!(monotonic_ns().unwrap() >= first);
        match host_id() {
            Ok(id) => assert_eq!(host_id().unwrap(), id),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                eprintln!("skipped: this host has no machine identity: {error}");
            }
            Err(error) => panic!("{error}"),
        }
    }

    /// The stores tell files apart by the identities that their seals hold, with the 128-bit file
    /// IDs of `FILE_ID_INFO` on Windows, whether they examine a path, an open file, or an entry of
    /// an open directory.
    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn the_stores_tell_files_apart_by_the_identities_that_their_seals_hold() {
        use crate::host::seal::FileSeal;

        let directory = tempfile::tempdir().unwrap();
        let opened = Directory::open(directory.path()).unwrap();
        let mut ids = Vec::new();
        for name in ["first", "second"] {
            let path = directory.path().join(name);
            std::fs::write(&path, b"").unwrap();
            let file = std::fs::File::open(&path).unwrap();
            let seal = FileSeal::of(&file).unwrap();
            let id = (seal.device(), seal.file_id());
            assert_eq!(opened_file_id(&file).unwrap(), id, "{name}");
            assert_eq!(file_id(&path).unwrap(), id, "{name}");
            assert_eq!(opened.entry_id(name).unwrap(), Some(id), "{name}");
            ids.push(id);
        }
        assert_ne!(ids[0], ids[1]);
        assert_eq!(opened.entry_id("missing").unwrap(), None);
    }
}
