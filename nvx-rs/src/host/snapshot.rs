//! Strict snapshot directories, compatibility keys, manifests and restore-time eligibility.
//!
//! [`Snapshot::open`] checks the one snapshot format without reading its informational
//! description. The caller keeps the directory and its entries private and immutable while
//! using the opened snapshot.
//!
//! [`finish_capture`] validates an exited capture and writes its key and informational
//! description without publishing or removing any VM-directory entry.
//!
//! [`SnapshotKey`] derives a compatibility key from a capture's class and manifest.
//! [`SnapshotManifest::read`] reads the manifest that OpenVMM wrote, and
//! [`TimeContract::eligibility`] checks its restore window at a [`HostTime`].
//!
//! # Canonical encoding
//!
//! A key and a class each have one canonical encoding, which [`SnapshotKey::to_bytes`] and
//! [`CaptureClass::to_bytes`] write, and whose SHA-256 digest, after a domain of its own, is its
//! identity, a [`KeyIdentity`] or a [`ClassIdentity`]. The encoding starts with its version, and
//! each value in it has exactly one encoding:
//!
//! - An integer is its fixed number of little-endian bytes: one for a `u8`, two for a `u16`, four
//!   for a `u32`, and eight for a `u64`.
//! - A boolean is one byte, 0 or 1.
//! - A byte string of fixed length, such as a digest or an address, is its bytes.
//! - A byte string of variable length is its length, a `u32`, then its bytes. A text is the byte
//!   string of its UTF-8 encoding.
//! - An optional value is one byte: 0 without the value, and 1 before it.
//! - A list is its count, a `u32`, then its items in order.
//!
//! [`SnapshotKey::from_bytes`] refuses any other encoding, and bytes left after it. The vectors
//! in `tests/vectors/snapshot-key-v1.txt` fix version 1 of a key's encoding and of a class's.

mod canonical;
mod capture;
pub(super) mod eligibility;
pub(super) mod key;
pub(super) mod manifest;
#[cfg(test)]
pub(crate) mod tests;
mod wire;

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::host::config::{CaptureConfig, ConsoleAttachment, NicConfig};
use crate::host::paths::{absolute, normalized};
use crate::host::runtime::RuntimeIdentity;
use crate::host::store::regular;
use crate::host::{filesystem, platform};
use crate::{Error, Hypervisor, Result};

pub use self::capture::{CapturedSnapshot, finish_capture};
pub(super) use self::capture::{check_exit, check_snapshot};
pub use self::eligibility::{Downtime, DowntimeSource, Eligibility, HostTime, MAX_DOWNTIME_NS};
pub use self::key::{CaptureClass, ClassIdentity, Invariants, KeyIdentity, SnapshotKey};
pub use self::manifest::{
    BlockGeometry, HostClock, MANIFEST_NAME, MAX_MANIFEST_LEN, SnapshotManifest, TimeContract,
};

/// The canonical snapshot key.
pub const KEY_FILE: &str = "key.bin";
/// The informational description, whose contents [`Snapshot::open`] never reads.
pub const DESCRIPTION_FILE: &str = "snapshot.json";
/// The directory of OpenVMM's manifest, RAM and saved state.
pub const OPENVMM_DIR: &str = "openvmm";

/// OpenVMM's exact platform-snapshot inventory, in name order.
pub(super) const SNAPSHOT_ARTIFACTS: [&str; 3] = [MANIFEST_NAME, "memory.bin", "state.bin"];

/// The checked configuration and effective devices of a blockless platform capture.
///
/// [`crate::host::vm::Prepared::capture_context`] returns it after verifying the actual sparse
/// disk geometries. Callers choose whether and how to persist it; creation writes no context,
/// key or snapshot description into the VM directory. Runtime paths and network policy are
/// not part of the context.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureContext {
    /// Capture resources, invariants and verified image and scratch geometries.
    pub(crate) capture: CaptureConfig,
    /// The attached NIC's resolved address, without the capture's deny-all policy.
    pub(crate) nic: Option<NicConfig>,
    /// The boot console's effective attachment.
    pub(crate) console: ConsoleAttachment,
    /// The identities of the runtime artifacts and their contracts.
    pub(crate) runtime: RuntimeIdentity,
    /// The backend selected for the capture.
    pub(crate) hypervisor: Hypervisor,
    /// The requested CPU profile, or OpenVMM's default.
    pub(crate) cpu_profile: Option<String>,
}

/// A checked snapshot directory and its verified metadata, for a future VM restore.
///
/// The caller must keep the directory and its entries private and immutable while using this
/// value. Opening checks them once; it neither watches them nor holds their files open.
#[derive(Debug)]
pub struct Snapshot {
    /// The absolute directory path, with its components preserved rather than its links resolved.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "SL2.15 uses the snapshot directory (#1103)")
    )]
    pub(crate) dir: PathBuf,
    /// The bounded canonical key checked against the manifest.
    key: SnapshotKey,
    /// The strictly parsed metadata of OpenVMM's snapshot.
    manifest: SnapshotManifest,
}

impl Snapshot {
    /// Opens the one strict snapshot format without changing any entry.
    ///
    /// `dir` must be a private directory containing exactly [`KEY_FILE`], [`DESCRIPTION_FILE`]
    /// and [`OPENVMM_DIR`]. The latter must be a private directory containing exactly
    /// [`MANIFEST_NAME`], `memory.bin` and `state.bin`. Every file must be regular, not a link or
    /// reparse point, and private to its owner. The manifest is read by
    /// [`SnapshotManifest::read`], its artifact lengths must match, and the bounded canonical
    /// key must be consistent with it. The description is required but its contents are never
    /// read, parsed or trusted.
    ///
    /// The caller must keep the directory and its entries private and immutable while using
    /// the returned snapshot. This check retains no open file handles or lock.
    ///
    /// Fails with [`ErrorKind::Io`](crate::ErrorKind::Io) for an unreadable or invalid directory,
    /// entry, length or key relationship, and otherwise as [`SnapshotManifest::read`] and
    /// [`SnapshotKey::from_bytes`] do.
    pub fn open(dir: &Path) -> Result<Self> {
        let dir = absolute(&normalized(dir)).map_err(|error| entry_error(dir, error))?;
        check_directory(&dir, &[KEY_FILE, DESCRIPTION_FILE, OPENVMM_DIR])?;
        for path in [dir.join(KEY_FILE), dir.join(DESCRIPTION_FILE)] {
            check_file(&path)?;
        }
        let manifest = check_snapshot(&dir.join(OPENVMM_DIR))?;
        let path = dir.join(KEY_FILE);
        let bytes = regular::open_regular(&path)
            .and_then(|file| {
                regular::read_at_most(file, SnapshotKey::MAX_LEN as u64, "a snapshot key")
            })
            .map_err(|error| entry_error(&path, error))?;
        let key = SnapshotKey::from_bytes(&bytes)?;
        key.check_manifest(&manifest)?;
        Ok(Self { dir, key, manifest })
    }

    /// Returns the verified snapshot key.
    pub fn key(&self) -> &SnapshotKey {
        &self.key
    }

    /// Returns the strictly parsed OpenVMM manifest.
    pub fn manifest(&self) -> &SnapshotManifest {
        &self.manifest
    }
}

/// Names the failing snapshot entry while retaining its I/O error.
fn entry_error(path: &Path, error: io::Error) -> Error {
    Error::io(format!("cannot examine snapshot entry {}", path.display())).with_source(error)
}

/// Checks one private directory and its exact fixed entry-name inventory.
fn check_directory(path: &Path, expected: &[&str]) -> Result<()> {
    if let Some(problem) =
        platform::check_snapshot_dir(path).map_err(|error| entry_error(path, error))?
    {
        return Err(Error::io(format!(
            "the snapshot directory {} {problem}",
            path.display()
        )));
    }
    let mut found = fs::read_dir(path)
        .and_then(|entries| {
            entries
                .map(|entry| Ok(entry?.file_name()))
                .collect::<io::Result<Vec<_>>>()
        })
        .map_err(|error| entry_error(path, error))?;
    let mut expected: Vec<OsString> = expected.iter().map(|name| OsString::from(*name)).collect();
    found.sort();
    expected.sort();
    if found != expected {
        return Err(Error::io(format!(
            "the snapshot directory {} holds {found:?}, not exactly {expected:?}",
            path.display()
        )));
    }
    Ok(())
}

/// Checks a regular private file from metadata alone, without reading its contents.
fn check_file(path: &Path) -> Result<fs::Metadata> {
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, READ_CONTROL,
        };

        fs::OpenOptions::new()
            .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(|error| entry_error(path, error))?
    };
    #[cfg(windows)]
    let metadata = file.metadata().map_err(|error| entry_error(path, error))?;
    #[cfg(not(windows))]
    let metadata = fs::symlink_metadata(path).map_err(|error| entry_error(path, error))?;
    if !metadata.is_file() || filesystem::is_reparse_point(&metadata) {
        return Err(Error::io(format!(
            "the snapshot entry {} is not a regular file, or is a link or reparse point",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if metadata.uid() != nix::unistd::Uid::effective().as_raw() || metadata.mode() & 0o077 != 0
        {
            return Err(Error::io(format!(
                "the snapshot file {} is not private to the current user",
                path.display()
            )));
        }
    }
    #[cfg(windows)]
    if let Some(problem) =
        platform::private_file_problem(&file).map_err(|error| entry_error(path, error))?
    {
        return Err(Error::io(format!(
            "the snapshot file {} {problem}",
            path.display()
        )));
    }
    Ok(metadata)
}
