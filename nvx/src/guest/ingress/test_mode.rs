//! Runs numbered procfs snapshots through the collector.

use std::ffi::{CStr, OsString};
use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use super::collect::{self, Collector, DiagnosticInput, IPV6_DUAL_STACK_BY_DEFAULT};
use super::{DecimalFile, FIRST_SOURCE_PORT, PATH_BUFFER_SIZE, bounded_path, read_decimal_file};
use crate::guest::sys;

/// The file of a snapshot that holds its scan time, in milliseconds, which test mode prints and
/// passes to the collector in place of the monotonic clock.
const TIME_FILE: &str = "/time";

/// Size of the C test mode's buffer for a snapshot's time path, `char path[4200]`.
///
/// It exceeds `PATH_BUFFER_SIZE` by more than the length of `TIME_FILE`, so the time path of
/// every root that fits its own buffer fits too, and the check that the port keeps for parity
/// never fails; a time path too long to open reads as a missing file, as it did in C.
const TIME_PATH_BUFFER_SIZE: usize = 4200;

/// Typed failure while traversing a snapshot sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Error {
    /// A generated path would not fit the matching C buffer.
    PathTooLong,
    /// Snapshot zero has no readable time file.
    MissingFirstSnapshot,
    /// A time file cannot be read, or holds anything but one decimal number.
    MalformedTime,
}

/// Runs every numbered snapshot and writes its canonical hexadecimal announcement.
pub(super) fn run(directory: &CStr, out: &mut dyn Write) -> Result<(), Error> {
    let mut collector = Collector::new();
    let mut index = 0_u32;
    loop {
        let root = snapshot_root(directory.to_bytes(), index).ok_or(Error::PathTooLong)?;
        let time_path =
            bounded_path(&root, TIME_FILE, TIME_PATH_BUFFER_SIZE).ok_or(Error::PathTooLong)?;
        let now = match read_decimal_file(&time_path) {
            DecimalFile::Value(now) => now,
            DecimalFile::Missing if index == 0 => return Err(Error::MissingFirstSnapshot),
            DecimalFile::Missing => return Ok(()),
            DecimalFile::Malformed => return Err(Error::MalformedTime),
        };
        let ipv6_dual_stack = collect::read_bindv6only(&root, IPV6_DUAL_STACK_BY_DEFAULT);
        let payload = collector.collect_files(
            &root,
            DiagnosticInput::Snapshot,
            FIRST_SOURCE_PORT,
            now,
            ipv6_dual_stack,
        );
        let mut line = format!("{now} ").into_bytes();
        for byte in payload {
            line.extend_from_slice(format!("{byte:02x}").as_bytes());
        }
        line.push(b'\n');
        sys::report(out, &[&line]);
        index = index.wrapping_add(1);
    }
}

/// Builds `DIRECTORY/INDEX` byte for byte as the C program's `"%s/%u"` did, if it fits the C
/// program's root buffer of `PATH_BUFFER_SIZE` bytes.
fn snapshot_root(directory: &[u8], index: u32) -> Option<PathBuf> {
    let index = index.to_string();
    if directory.len() + 1 + index.len() >= PATH_BUFFER_SIZE {
        return None;
    }
    let mut root = Vec::with_capacity(directory.len() + 1 + index.len());
    root.extend_from_slice(directory);
    root.push(b'/');
    root.extend_from_slice(index.as_bytes());
    Some(PathBuf::from(OsString::from_vec(root)))
}

#[cfg(test)]
mod tests;
