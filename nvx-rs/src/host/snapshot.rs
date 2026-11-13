//! Strict snapshot directories, compatibility keys, manifests and restore-time eligibility.
//!
//! [`Snapshot::open`] checks the one snapshot format without reading its informational
//! description. The caller keeps the directory and its entries private and immutable while
//! using the opened snapshot.
//!
//! [`finish_capture`] validates an exited capture and writes its key and informational
//! description without publishing or removing any VM-directory entry.
//!
//! [`SnapshotKey`] derives a compatibility key from a capture's context and manifest.
//! [`SnapshotManifest::read`] reads the manifest that OpenVMM wrote, and
//! [`TimeContract::eligibility`] checks its restore window at a [`HostTime`].
//! [`Snapshot::accepts`] checks a requested VM against that verified metadata.
//!
//! # Canonical encoding
//!
//! A key has one canonical encoding, which [`SnapshotKey::to_bytes`] writes. Its [`KeyIdentity`]
//! digests its compatibility prefix, excluding the appended capture-time anchors and identities,
//! with SHA-256 after a domain of its own. The encoding starts with its version, and each value in
//! it has exactly one encoding:
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
//! in `tests/vectors/snapshot-key-v1.txt` fix version 1 of a key's encoding.

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

use crate::host::config::{
    CaptureConfig, ConsoleAttachment, NicConfig, PROCESSOR_COUNTS, RootConfig, UpperConfig,
    VmConfig,
};
use crate::host::paths::{absolute, normalized};
use crate::host::runtime::{Hypervisor, Runtime, RuntimeIdentity};
use crate::host::{filesystem, platform, sandbox, seal};
use crate::net::Ipv4Cidr;
use crate::{Error, Result};

pub(super) use self::capture::check_snapshot;
pub use self::capture::{CapturedSnapshot, finish_capture};
pub use self::eligibility::{Downtime, DowntimeSource, Eligibility, HostTime, MAX_DOWNTIME_NS};
pub use self::key::{Invariants, KeyIdentity, SnapshotKey};
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

/// A checked snapshot directory and its verified metadata, which
/// [`vm::create`](crate::host::vm::create) restores a VM from.
///
/// The caller must keep the directory and its entries private and immutable while using this
/// value. Opening checks them once; it neither watches them nor holds their files open.
#[derive(Debug)]
pub struct Snapshot {
    /// The absolute directory path, with its components preserved rather than its links resolved.
    pub(crate) dir: PathBuf,
    /// The bounded canonical key checked against the manifest.
    key: SnapshotKey,
    /// The strictly parsed metadata of OpenVMM's snapshot.
    manifest: SnapshotManifest,
    /// The captured NIC's checked address, without a network policy.
    nic: Option<NicConfig>,
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
    /// entry, length, key relationship or captured NIC address, and otherwise as
    /// [`SnapshotManifest::read`] and [`SnapshotKey::from_bytes`] do.
    pub fn open(dir: &Path) -> Result<Self> {
        let dir = absolute(&normalized(dir)).map_err(|error| entry_error(dir, error))?;
        check_directory(&dir, &[KEY_FILE, DESCRIPTION_FILE, OPENVMM_DIR])?;
        for path in [dir.join(KEY_FILE), dir.join(DESCRIPTION_FILE)] {
            check_file(&path)?;
        }
        let manifest = check_openvmm(&dir.join(OPENVMM_DIR))?;
        let path = dir.join(KEY_FILE);
        let bytes = platform::open_regular(&path)
            .and_then(|file| {
                platform::read_at_most(file, SnapshotKey::MAX_LEN as u64, "a snapshot key")
            })
            .map_err(|error| entry_error(&path, error))?;
        let key = SnapshotKey::from_bytes(&bytes)?;
        key.check_manifest(&manifest)?;
        let nic = manifest
            .machine
            .nic
            .as_ref()
            .map(|nic| {
                Ipv4Cidr::new(nic.ipv4, nic.prefix_length)
                    .map(|guest_network| NicConfig { guest_network })
                    .map_err(|problem| {
                        Error::io(format!(
                            "the snapshot's captured NIC address is invalid: {problem}"
                        ))
                    })
            })
            .transpose()?;
        Ok(Self {
            dir,
            key,
            manifest,
            nic,
        })
    }

    /// Returns the verified snapshot key.
    pub fn key(&self) -> &SnapshotKey {
        &self.key
    }

    /// Returns the strictly parsed OpenVMM manifest.
    pub fn manifest(&self) -> &SnapshotManifest {
        &self.manifest
    }

    /// Returns the captured NIC's presence and checked address, without a network policy.
    ///
    /// An unnamed restore address inherits this value. This accessor reads no snapshot entry.
    pub fn nic(&self) -> Option<NicConfig> {
        self.nic
    }

    /// Links the fixed snapshot inventory into a newly created restore view.
    ///
    /// The caller owns `destination` and records each successful link for rollback. Hard-link
    /// errors, including a different filesystem, retain their source; nothing is copied.
    pub(crate) fn link_restore_view(
        &self,
        destination: &Path,
        created: &mut Vec<PathBuf>,
    ) -> Result<()> {
        for name in SNAPSHOT_ARTIFACTS {
            let source = self.dir.join(OPENVMM_DIR).join(name);
            let path = destination.join(name);
            fs::hard_link(&source, &path).map_err(|error| {
                Error::io(format!(
                    "cannot hard-link snapshot artifact {} into restore view {}",
                    source.display(),
                    path.display()
                ))
                .with_source(error)
            })?;
            created.push(path);
        }
        Ok(())
    }

    /// Checks whether this snapshot can restore `config` under `runtime` at `now`.
    ///
    /// Requires a layered root, processors from the effective boot count to capacity in
    /// counts of 1, 2, 4 or 8, and memory from boot to capacity in multiples of 128 MiB.
    /// An encoded boot processor count of zero means the entire capacity is already online.
    /// The image must fit the placeholder with the same logical and physical block sizes;
    /// a scratch upper must have the dormant scratch's complete geometry.
    ///
    /// NIC presence must match. A named address must equal the captured address; an unnamed
    /// one inherits it. Network policies are not compared. The boot console must keep its
    /// captured attachment policy: `recreate-listener` for a listener, or
    /// `discard-while-disconnected` for a disconnected console. Neither its device nor the
    /// control console proves this attachment. The runtime must use the captured backend and
    /// CPU profile, and have the same runtime identity. An omitted profile or `auto` names
    /// the captured profile; `host` also names it when that profile is host-derived.
    ///
    /// Reads only the requested image and scratch metadata, never the snapshot entries again.
    /// The caller keeps the snapshot private and immutable, as [`open`](Self::open) requires.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for a compatibility
    /// refusal, with [`ErrorKind::Unavailable`](crate::ErrorKind::Unavailable) for an ineligible
    /// time contract, and with [`ErrorKind::Io`](crate::ErrorKind::Io) for disk I/O failures.
    pub fn accepts(&self, config: &VmConfig, runtime: &Runtime, now: &HostTime) -> Result<()> {
        config.check()?;
        let RootConfig::Layered(root) = &config.root else {
            return Err(Error::unsupported(
                "a snapshot restore requires a layered root",
            ));
        };
        let capacity = self.key.processors();
        let boot = match self.key.boot_processors() {
            0 => capacity,
            boot => boot,
        };
        if !PROCESSOR_COUNTS.contains(&config.processors)
            || config.processors < boot
            || config.processors > capacity
        {
            return Err(Error::unsupported(format!(
                "snapshot processors must be 1, 2, 4 or 8, from boot {boot} to capacity \
                 {capacity}, not {}",
                config.processors
            )));
        }
        let memory = config.memory_mib.checked_mul(1024 * 1024).ok_or_else(|| {
            Error::unsupported("snapshot memory in MiB overflows its byte length")
        })?;
        if memory < self.key.memory() || memory > self.key.memory_capacity() {
            return Err(Error::unsupported(format!(
                "snapshot memory must be from boot {} to capacity {} bytes, not {memory}",
                self.key.memory(),
                self.key.memory_capacity()
            )));
        }
        if !config.memory_mib.is_multiple_of(128) {
            return Err(Error::unsupported(
                "snapshot memory must be aligned to 128 MiB",
            ));
        }
        let image = restore_geometry(&root.image, "image")?;
        let placeholder = self.key.distro();
        if image.length > placeholder.length {
            return Err(Error::unsupported(format!(
                "snapshot image length {} exceeds the placeholder's {} bytes",
                image.length, placeholder.length
            )));
        }
        if image.logical_block_size != placeholder.logical_block_size
            || image.physical_block_size != placeholder.physical_block_size
        {
            return Err(Error::unsupported(format!(
                "snapshot image block sizes {image:?} differ from the placeholder's \
                 {placeholder:?}"
            )));
        }
        if let UpperConfig::Scratch { path, .. } = &root.upper {
            let scratch = restore_geometry(path, "scratch")?;
            if scratch != self.key.scratch() {
                return Err(Error::unsupported(format!(
                    "snapshot scratch geometry {scratch:?} differs from the dormant scratch's {:?}",
                    self.key.scratch()
                )));
            }
        }
        match (&config.network, self.nic()) {
            (None, Some(_)) | (Some(_), None) => {
                return Err(Error::unsupported(
                    "snapshot NIC presence differs from the requested VM's",
                ));
            }
            (Some(network), Some(captured)) => {
                if network
                    .guest_network
                    .is_some_and(|address| address != captured.guest_network)
                {
                    return Err(Error::unsupported(format!(
                        "snapshot NIC address must remain {}",
                        captured.guest_network
                    )));
                }
            }
            (None, None) => {}
        }
        let console = self
            .manifest
            .attachments
            .iter()
            .find(|attachment| attachment.stable_id == "console:microvm-virtio0");
        let policy = match config.console {
            ConsoleAttachment::Listener => "recreate-listener",
            ConsoleAttachment::Disconnected => "discard-while-disconnected",
        };
        if console.is_none_or(|attachment| attachment.reconnect_policy != policy) {
            return Err(Error::unsupported(
                "snapshot boot console attachment policy does not match the requested console",
            ));
        }
        if runtime.hypervisor != self.key.hypervisor() {
            return Err(Error::unsupported(
                "snapshot backend differs from the runtime's hypervisor",
            ));
        }
        let profile = self.key.cpu_profile();
        if !key::cpu_profile_matches(
            &self.manifest.machine.cpu,
            runtime.cpu_profile.as_deref().unwrap_or("auto"),
        ) {
            return Err(Error::unsupported(format!(
                "snapshot CPU profile must remain {profile}"
            )));
        }
        if &runtime.identity != self.key.runtime() {
            return Err(Error::unsupported(format!(
                "snapshot runtime identity differs in {:?}",
                self.key.runtime().differences(&runtime.identity)
            )));
        }
        let eligibility = self.manifest.time().eligibility(now);
        if !eligibility.is_eligible() {
            return Err(Error::unavailable(format!(
                "snapshot time contract is {eligibility}"
            )));
        }
        Ok(())
    }
}

/// Reads a requested regular disk's geometry without waiting for a Linux FIFO writer.
fn restore_geometry(path: &Path, description: &str) -> Result<BlockGeometry> {
    let file = seal::open_read(path).map_err(|error| {
        Error::io(format!(
            "cannot open snapshot restore {description} {}",
            path.display()
        ))
        .with_source(error)
    })?;
    let metadata = file.metadata().map_err(|error| {
        Error::io(format!(
            "cannot examine snapshot restore {description} {}",
            path.display()
        ))
        .with_source(error)
    })?;
    if !metadata.is_file() {
        return Err(Error::unsupported(format!(
            "the snapshot restore {description} {} is not a regular file",
            path.display()
        )));
    }
    sandbox::block_geometry(&file, path, description)
}

/// Adds the snapshot format's per-file owner-only checks to the legacy capture checks.
fn check_openvmm(path: &Path) -> Result<SnapshotManifest> {
    let manifest = check_snapshot(path)?;
    for name in SNAPSHOT_ARTIFACTS {
        check_file(&path.join(name))?;
    }
    Ok(manifest)
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
