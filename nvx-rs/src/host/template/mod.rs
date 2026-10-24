//! Templates: captures of a microVM from which independent clones are restored, the metadata
//! that decides which clones a template can serve, and on which hosts, and the store that keeps
//! them.
//!
//! A template is a snapshot that OpenVMM captured at its platform tier, after it consumed the
//! invariant configuration of a microVM alone, for clones: a VM booted with a placeholder
//! `distro`, a fresh scratch, and a dormant filesystem slot, which each clone binds to its own
//! image, scratch, and export when it is restored. Its compatibility key, [`TemplateKey`], is what
//! a clone and the host that restores it must match: the runtime and the contracts through which
//! the template is restored, and the machine that it restores.
//!
//! - [`CaptureClass`] holds the invariant inputs of a capture, known before it: the runtime, the
//!   guest's build, the backend, the CPU profile, the resources, the geometries of the disks, the
//!   network, and the other invariant settings.
//! - [`TemplateManifest::read`] reads the manifest that OpenVMM wrote for a capture, with its time
//!   contract, and refuses one that is not a template's.
//! - [`TemplateKey::new`] derives the key of a template from its class and its manifest, after it
//!   checks that the capture made the machine that its class requested.
//! - [`Store`] holds the generations of templates that a host published, apart from every
//!   sandbox's store: their payloads, which never change, and the versioned [`Index`] of them,
//!   which names each by its [`StoreGeneration`](crate::activation::StoreGeneration), at a
//!   location that the adapter chooses.
//!
//! # Canonical encoding
//!
//! A key and a class each have one canonical encoding, which [`TemplateKey::to_bytes`] and
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
//! [`TemplateKey::from_bytes`] refuses any other encoding, and bytes left after it. The vectors in
//! `tests/vectors/template-key-v1.txt` fix version 1 of a key's encoding.

mod canonical;
mod files;
mod key;
mod manifest;
mod store;
#[cfg(test)]
mod test_support;
mod wire;

pub use self::key::{CaptureClass, ClassIdentity, Invariants, KeyIdentity, TemplateKey};
pub use self::manifest::{
    BlockGeometry, HostClock, MANIFEST_NAME, MAX_MANIFEST_LEN, TemplateManifest, TimeContract,
};
pub use self::store::{
    GENERATIONS_NAME, Generation, INDEX_LOCK_NAME, INDEX_NAME, Index, KEY_NAME, MAX_GENERATIONS,
    SNAPSHOT_NAME, STORE_FORMAT, STORE_RECORD_NAME, StorageGeneration, Store, StoreId,
};
