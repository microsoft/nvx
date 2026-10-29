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
//! - [`Store::create_template`] and [`Store::refresh_template`] capture a template of a class, in
//!   the VM that its [`TemplatePlan`] describes, which the supervisor launches, and publish it as
//!   a generation of the store; [`Store::recover_captures`] ends the captures that a crash
//!   interrupted.
//! - [`TimeContract::eligibility`] decides whether a clone may restore a capture at a host's
//!   time, [`HostTime`], by the restore window of its time contract, apart from its
//!   compatibility.
//! - [`Store::acquire`] leases an eligible generation to a clone, with a binding in the clone's
//!   sandbox, until [`Store::release`] ends the lease once the clone's VM has exited. A tombstone
//!   quarantines a generation, which takes no new lease, and [`Store::retire`] deletes it once its
//!   leases have drained.
//! - [`ClonePlan::new`] plans the clone of a leased generation, as a [`CloneRequest`] asks, under
//!   the lock that acquired the lease, before the clone's launch: it checks the clone's image
//!   against its binding, its upper, in RAM or on the caller's scratch image, which it claims, its
//!   memory target against the template, and its export, if it has one, an aggregate of host
//!   directories and files, as a cold launch exports it, which the template's dormant filesystem
//!   slot binds, and refuses a request that the template cannot serve, such as an image longer
//!   than the template's placeholder, the capacity of every clone's image, or of other block
//!   sizes, or a network that attaches a NIC where the template has none, none where it has one,
//!   or one at another guest address than the template's, before any VMM starts.
//! - [`ClonePlan::launch`] launches the clone, as the launch that its lease reserves, once the
//!   objects of its export are still those that its plan found: OpenVMM restores the generation's
//!   snapshot in place with the clone's devices and export, and the clone's activation record
//!   waits in its sandbox, as a [`Restoring`] describes.
//! - [`Restoring::activate`] runs the clone's restore sequence through the adapter's framing of
//!   the clone's control session, a [`CloneControl`]: it waits for OpenVMM's restore-ready event,
//!   then connects, sends the clone's activation record in the adapter's Activate, and checks the
//!   activation that the clone's Ready reports, by a deadline that starts with the launch, and
//!   resends the same record in a new session once the adapter lost one. [`Activated::start`]
//!   then sends the clone's mapping table, if it has an export, and starts the clone's sandbox
//!   once the guest applied every mapping, by a deadline that starts at its Ready, and
//!   [`Started::commit`] commits the launch.
//! - The plan, the launch, and the restore sequence fail with a [`CloneFailure`], whose
//!   [`Evidence`] attributes it to an [`Outcome`], at a [`Stage`], once the clone got as far as
//!   its [`Progress`], with the [`Teardown`] that followed: a failure from the launch on ends the
//!   clone's VMM through its verified identity, and is recorded in the clone's sandbox, which
//!   keeps its lease while a VM of the clone may run. Only a failure of the template quarantines
//!   the leased generation, through its tombstone, [`CloneFailure::quarantine`].
//! - [`reattach_clone`] takes a clone over once the caller of its launch restarted: it checks the
//!   clone's activation in a new session, and ends a clone that another runtime identity
//!   launched, or that fails its check, or classifies how it ended; [`CloneRecords::read`] reads
//!   what the clone's sandbox keeps of it, for the operations that list, inspect, or remove
//!   clones.
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
//! `tests/vectors/template-key-v1.txt` fix version 1 of a key's encoding and of a class's.

mod canonical;
mod clone;
mod eligibility;
mod key;
mod manifest;
mod plan;
mod store;
#[cfg(test)]
mod test_support;
mod wire;

pub use self::clone::{
    Activated, CloneControl, CloneFailure, ClonePlan, CloneRecords, CloneRequest, CloneUpper,
    Evidence, FAILURE_FORMAT, FailureRecord, MAX_ACTIVATION_SESSIONS, Outcome, Progress,
    READY_TO_START_TIMEOUT, RESTORE_READY_EVENT, RESTORE_TO_READY_TIMEOUT, ReattachedClone,
    RestoreDeadlines, Restoring, Stage, Started, Teardown, reattach_clone, status_evidence,
};
pub use self::eligibility::{Downtime, DowntimeSource, Eligibility, HostTime, MAX_DOWNTIME_NS};
pub use self::key::{CaptureClass, ClassIdentity, Invariants, KeyIdentity, TemplateKey};
pub use self::manifest::{
    BlockGeometry, HostClock, MANIFEST_NAME, MAX_MANIFEST_LEN, TemplateManifest, TimeContract,
};
pub use self::plan::{MEMORY_BLOCK_SIZE, SCRATCH_NAME, TemplatePlan, TemplateResources};
pub use self::store::{
    Acquired, CAPTURE_RECORD_NAME, CAPTURES_NAME, Capture, Created, GENERATIONS_NAME, Generation,
    INDEX_LOCK_NAME, INDEX_NAME, Index, KEY_NAME, LEASES_NAME, Lease, LeaseId, MAX_GENERATIONS,
    Retirement, SNAPSHOT_NAME, STORE_FORMAT, STORE_RECORD_NAME, StorageGeneration, Store, StoreId,
    TOMBSTONES_NAME, TRANSACTION_NAME,
};
