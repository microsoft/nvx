//! Snapshot metadata: compatibility keys, strict manifests and restore-time eligibility.
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
pub(super) mod eligibility;
pub(super) mod key;
pub(super) mod manifest;
#[cfg(test)]
pub(crate) mod tests;
mod wire;

pub use self::eligibility::{Downtime, DowntimeSource, Eligibility, HostTime, MAX_DOWNTIME_NS};
pub use self::key::{CaptureClass, ClassIdentity, Invariants, KeyIdentity, SnapshotKey};
pub use self::manifest::{
    BlockGeometry, HostClock, MANIFEST_NAME, MAX_MANIFEST_LEN, SnapshotManifest, TimeContract,
};
