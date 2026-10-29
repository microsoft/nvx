//! The store of templates, [`Store`]: its directory, its records, the namespaces of its
//! generations, the captures that publish them, and their leases, tombstones, and retirement.

mod capture;
mod leases;
#[cfg(test)]
mod tests;

use std::fmt;
use std::fs::{self, File, TryLockError};
use std::io;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use super::key::{ClassIdentity, KeyIdentity, TemplateKey};
use super::manifest::{HostClock, TimeContract};
use crate::activation::StoreGeneration;
use crate::error::{Error, ErrorCode, ErrorKind, Result};
use crate::host::platform::{self, parse_hex_id};
use crate::host::process::BootId;
use crate::host::store::files::{open_lock, write_private};
use crate::host::store::lease::{
    ID_LEN, StoreRecord, hex, read_identity, read_private, read_record, store_error,
};

#[cfg(all(test, any(target_os = "linux", windows)))]
pub(super) use self::capture::SNAPSHOT_ARTIFACTS;
pub(super) use self::capture::SOCKET_PATH_LIMIT;
pub use self::capture::{CAPTURE_RECORD_NAME, CAPTURES_NAME, Capture, Created, TRANSACTION_NAME};
pub use self::leases::{Acquired, LEASES_NAME, Retirement, TOMBSTONES_NAME};
pub use crate::host::store::lease::{
    GENERATIONS_NAME, Lease, LeaseId, STORE_FORMAT, STORE_RECORD_NAME, StoreId,
};

/// Name of the store's index.
pub const INDEX_NAME: &str = "index.json";
/// Name of the lock of the store's index.
pub const INDEX_LOCK_NAME: &str = "index.lock";
/// Name of the canonical encoding of a generation's key in its payload.
pub const KEY_NAME: &str = "key.bin";
/// Name of a generation's snapshot in its payload.
pub const SNAPSHOT_NAME: &str = "snapshot";
/// Most generations that a store holds.
pub const MAX_GENERATIONS: usize = 1024;

/// The store generation that a new store assigns first.
const FIRST_GENERATION: StoreGeneration = StoreGeneration::new(NonZeroU64::MIN);

/// Longest index, in bytes: more than an index of [`MAX_GENERATIONS`] generations takes.
const MAX_INDEX_LEN: u64 = 4 * 1024 * 1024;

/// Interval at which a lock that another holder has is tried again, by a caller that waits for it
/// until a deadline.
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(25);

/// Draws 16 random bytes, for `what`.
fn random_id(what: &str) -> Result<[u8; ID_LEN]> {
    let mut bytes = [0; ID_LEN];
    getrandom::fill(&mut bytes)
        .map_err(|error| Error::other(format!("cannot draw {what}")).with_source(error))?;
    Ok(bytes)
}

/// The bound-block storage generation of a capture: 16 bytes, not all zero, that a capture gives
/// OpenVMM for every block that it binds, which the storage owner keeps immutable for those
/// blocks, so that neither the capture nor a restore reads a block to identify it.
///
/// It belongs to the storage owner, and is neither a [store generation](StoreGeneration), nor a
/// time lineage counter, nor a VM generation ID; see
/// [the four namespaces](Store#four-namespaces). Generations may share one. Its text form, which
/// `Display` writes and `FromStr` parses, is 32 lowercase hexadecimal digits, as OpenVMM's
/// `--snapshot-generation-id` takes it; parsing also accepts uppercase ones.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct StorageGeneration([u8; ID_LEN]);

impl StorageGeneration {
    /// Draws a fresh storage generation at random.
    pub fn generate() -> Result<Self> {
        loop {
            if let Some(generation) = Self::from_bytes(random_id("a storage generation")?) {
                return Ok(generation);
            }
        }
    }

    /// Returns the storage generation whose bytes are `bytes`, unless they are all zero, which no
    /// storage generation is.
    pub fn from_bytes(bytes: [u8; ID_LEN]) -> Option<Self> {
        (bytes != [0; ID_LEN]).then_some(Self(bytes))
    }

    /// Returns the bytes of the storage generation.
    pub const fn as_bytes(&self) -> &[u8; ID_LEN] {
        &self.0
    }
}

impl fmt::Display for StorageGeneration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&hex(&self.0))
    }
}

impl fmt::Debug for StorageGeneration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "StorageGeneration({self})")
    }
}

impl FromStr for StorageGeneration {
    type Err = Error;

    /// Parses the text form of a storage generation, failing with [`ErrorKind::Unsupported`] for
    /// anything else, all zeros included.
    fn from_str(text: &str) -> Result<Self> {
        parse_hex_id(text)
            .and_then(Self::from_bytes)
            .ok_or_else(|| {
                Error::unsupported(format!(
                    "{text:?} is not a storage generation, which is 32 hexadecimal digits, not \
                     all zero"
                ))
            })
    }
}

/// One generation of a template that a store holds, as its index lists it: its store generation,
/// the identities of its key and of its capture class, its storage generation, and what its
/// manifest records of the capture itself, in which the generations of one key differ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generation {
    /// Its store generation.
    generation: StoreGeneration,
    /// The identity of its key.
    key: KeyIdentity,
    /// The identity of its capture class.
    class: ClassIdentity,
    /// The storage generation of its capture.
    storage: StorageGeneration,
    /// When OpenVMM created its snapshot.
    created_at: SystemTime,
    /// The time contract of its capture.
    time: TimeContract,
    /// The length of its saved state, in bytes.
    state_len: u64,
    /// The length of its RAM, in bytes.
    memory_len: u64,
}

impl Generation {
    /// Returns the store generation, which names the generation in its store.
    pub fn generation(&self) -> StoreGeneration {
        self.generation
    }

    /// Returns the identity of the generation's compatibility key, which
    /// [`Store::read_key`] reads in full.
    pub fn key(&self) -> KeyIdentity {
        self.key
    }

    /// Returns the identity of the class of the capture that made the generation.
    pub fn class(&self) -> ClassIdentity {
        self.class
    }

    /// Returns the storage generation of the capture that made the generation.
    pub fn storage_generation(&self) -> StorageGeneration {
        self.storage
    }

    /// Returns when OpenVMM created the generation's snapshot.
    pub fn created_at(&self) -> SystemTime {
        self.created_at
    }

    /// Returns the time contract of the capture, with its anchor, host, boot, and time lineage
    /// counter.
    pub fn time(&self) -> &TimeContract {
        &self.time
    }

    /// Returns the length of the snapshot's saved state, in bytes.
    pub fn state_len(&self) -> u64 {
        self.state_len
    }

    /// Returns the length of the snapshot's RAM, in bytes.
    pub fn memory_len(&self) -> u64 {
        self.memory_len
    }
}

/// The index of a store, as one read found it: its generations, in the order of their
/// publication, and the store generation that its next publication takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    /// The store generation that the next publication takes.
    next: StoreGeneration,
    /// The generations, in the order of their store generations.
    generations: Vec<Generation>,
}

impl Index {
    /// Returns the generations, in the order of their publication.
    pub fn generations(&self) -> &[Generation] {
        &self.generations
    }

    /// Returns the generation of store generation `generation`, if the index lists it.
    pub fn get(&self, generation: StoreGeneration) -> Option<&Generation> {
        self.generations
            .binary_search_by_key(&generation, Generation::generation)
            .ok()
            .map(|index| &self.generations[index])
    }

    /// Returns the generations whose key has the identity `key`, in the order of their
    /// publication.
    pub fn of_key(&self, key: KeyIdentity) -> impl Iterator<Item = &Generation> {
        self.generations
            .iter()
            .filter(move |generation| generation.key == key)
    }

    /// Returns the store generation that the store's next publication takes: one past every store
    /// generation that it ever assigned.
    pub fn next_generation(&self) -> StoreGeneration {
        self.next
    }
}

/// A store of templates: a directory that the adapter chooses, apart from every sandbox's
/// store, with the generations of templates that its host published, and their index.
///
/// ```text
/// <dir>/                   the store, private to the current user
///   store.json             the store's identity record, which its creation writes last
///   index.json             the index of the generations that the store holds
///   index.lock             the lock that serializes the transactions of the index
///   generations/           the payloads of the generations, which never change once indexed
///     <G>/                 the payload of the store generation G
///       capture.json       the record of the capture that made it
///       key.bin            the canonical encoding of its compatibility key
///       snapshot/          its snapshot, as OpenVMM published it
///       <launch>.sock      on Linux, the control socket of the VMM of a clone that leases it,
///                          named after the clone's launch, until the lease's release
///   leases/                the records of the leases that the store granted
///     <L>.json             the record of the lease L
///   tombstones/            the tombstones of the generations that take no new leases
///     <G>.json             the tombstone of the store generation G
///   captures/              the slots of the capture classes
///     <C>/                 the slot of the classes whose identities start with the digits C,
///                          a sandbox store whose lock serializes the classes' captures
///       openvmm.log        the log of the VMM of the slot's latest capture
///       outcome.json       the outcome report of the VMM of the slot's latest capture
///       transaction/       the capture in progress: the sandbox store of its VM until the VM has
///                          ended, and then the payload of a generation
///         capture.json     the record of the capture
///         scratch.raw      the dormant scratch device, which the capture removes once checked
///         snapshot/        the snapshot, once OpenVMM has published it
///         key.bin          the template's key, once the capture has checked the snapshot
/// ```
///
/// # Identity
///
/// A store has an identity of its own, a [`StoreId`] that its creation draws, which its identity
/// record and its index both hold, so that the index of another store is refused.
/// [`Store::create`] creates a store, or opens the one that its directory already holds, and
/// [`Store::open`] opens an existing one.
///
/// # The index
///
/// The index lists the store's generations, in the order of their publication, with the metadata
/// of each that restores and their eligibility check without reading its payload: the identities
/// of its key and of its capture class, its storage generation, and what its manifest records of
/// the capture itself, its creation time, its time contract, and the lengths of its artifacts.
/// The payload of a generation never changes once the index lists it: the record of its capture,
/// the canonical encoding of its key, which [`Store::read_key`] reads, and its snapshot, which its
/// clones restore in place. Only the control sockets of the VMMs of the clones that lease it come
/// and go beside them, on Linux, as [`Store::acquire`] describes. The index
/// changes as generations are published and retired, and the records of leases and tombstones as
/// clones lease generations, and as generations are quarantined and retired.
///
/// The store's records, its identity record, its index, and the records of its leases and
/// tombstones, are JSON in format [`STORE_FORMAT`].
/// A record in any other format fails with a message that names its format, and is neither read
/// nor migrated: a store of another format is replaced by a new one, whose templates are captured
/// again. Records are replaced atomically: written to a temporary file beside them, flushed to
/// disk, and renamed over them.
///
/// # Four namespaces
///
/// Four numbers name generations of different things, and the store keeps them apart:
///
/// - The store generation, [`StoreGeneration`], is the host's publication ID: the store assigns
///   one to each generation as it indexes it, from 1 on, in the order of publication, and never
///   assigns one twice, even once its generation is gone. A clone's activation record names the
///   generation that it restores by its store generation and the identity of its key, in its
///   [`TemplateReference`](crate::activation::TemplateReference).
/// - The storage generation, [`StorageGeneration`], is the bound-block storage generation that a
///   capture gives OpenVMM, which the storage owner keeps immutable for the blocks that it binds.
///   It belongs to the storage owner, not to the store, and generations may share one.
/// - The time lineage counter, [`TimeContract::lineage`], is the generation counter of the
///   captured VM process, which each restore of a snapshot increments, so that the clones of one
///   generation share it.
/// - The VM generation ID is 16 random bytes that each VM process draws, which tell the clones of
///   one generation apart. Their guests read it, and the store records none.
///
/// # Captures and publication
///
/// [`Store::create_template`] and [`Store::refresh_template`] capture templates of a
/// [`CaptureClass`](crate::host::template::CaptureClass), and publish them as the store's
/// generations: a creation serves the class with a generation that the store holds, if one is
/// eligible, and captures one otherwise, while a refresh always captures. Each class has a slot in
/// `captures/`, a sandbox store named by the first 16 hexadecimal digits of the class's identity,
/// whose lock serializes the captures of the class, and which keeps the log and the outcome report
/// of the VMM of the class's latest capture. A capture holds its slot's lock until its VM has
/// ended, and takes the store's lock under it to publish, as the supervisor's
/// [lock order](crate::host::supervisor#the-lock-order) allows.
///
/// A capture runs in a transaction, a directory private to the current user, in its slot, on the
/// store's volume. It writes the record of the capture there, `capture.json`, which names the
/// store, the class, and the [`StorageGeneration`] that it drew, and creates the template's
/// dormant scratch device, a sparse file, then lowers the adapter's plan to a
/// [`TemplatePlan`](crate::host::template::TemplatePlan), whose snapshot destination is in the
/// transaction. The transaction is also the sandbox store of the capture's VM, whose launch
/// journal records its VMM: OpenVMM requires the control socket of a VM that captures a snapshot,
/// on Linux, beside the snapshot's destination, and a sandbox store's VM listens in the store's
/// directory. The capture holds the transaction's lock, under its slot's, until the VM has ended,
/// and releases it before it publishes. It cold-boots the template's VM through
/// [`supervisor::launch`](crate::host::supervisor::launch), from the transaction, as a launch of
/// its own, and waits for the VMM to exit: the guest captures itself, and OpenVMM publishes the
/// snapshot at its destination, and exits. The capture then moves the VMM's log and outcome
/// report to its slot.
///
/// ## Block devices by path
///
/// OpenVMM opens the block devices of a VM whose snapshot it captures, or restores, itself, at the
/// paths that its command line names: without following a link, nor on Windows any other reparse
/// point, and sharing only reads and deletion, so that its open fails while another handle may
/// write the file. The host therefore hands OpenVMM the paths of these files, never handles, and
/// checks the file at each path just before the VM starts:
///
/// - The directory that holds the VM's files, the capture's transaction, must be private to the
///   current user.
/// - The placeholder must pass its binding's check,
///   [`DistroBinding::check`](crate::host::sandbox::distro::DistroBinding::check), of its
///   identity and length, and then, at its path, be a regular file, not a link nor any other
///   reparse point, that is the file that the check found, that belongs to the current user or to
///   an administrator, and that no one else may change.
/// - The scratch must be, at its path, the regular file of the template's length that the capture
///   created, which no one else may change.
///
/// The host tells these files apart by the identity that their seals record,
/// [`FileSeal`](crate::host::seal::FileSeal): their device or volume and their file ID, on Windows
/// the 128-bit ID of `FILE_ID_INFO`, which a file system such as ReFS keeps unique where the
/// 64-bit file index is not.
///
/// As the VM starts, the host holds no handle through which it could write either file: it closes
/// the scratch once it has checked it, and holds only the placeholder that its binding's check
/// opened for reading, which on Windows lets no one write it, until the VM has ended. The
/// directories above the files must be ones that only the current user, or an administrator, may
/// change, since OpenVMM resolves each path again. A restore of a clone, whose block devices
/// OpenVMM also opens by their paths, takes the same rule; a restore of a clone in RAM declares its
/// scratch absent, which OpenVMM attaches with no file.
///
/// ## What the VM leaves
///
/// Once the VMM has exited, and before anything is published, the capture checks that the VM's
/// outcome report records the capture's success, and that the snapshot is a directory private to
/// the current user, as OpenVMM creates it, whose manifest
/// [`TemplateManifest::read`](crate::host::template::TemplateManifest::read) reads, whose artifacts
/// are exactly its manifest, RAM, and saved state, each a regular file private to the current
/// user, of the length that the manifest records, and whose key, [`TemplateKey::new`], is its
/// class's. It then closes the lock of the transaction's sandbox store, removes the lock's file,
/// the scratch, and what OpenVMM left beside the snapshot, and writes the key, `key.bin`, which
/// marks the transaction as checked, durably.
///
/// ## Two commits
///
/// A capture publishes its checked transaction under the store's lock, in two commits: it renames
/// the transaction to the payload of the index's next store generation, which no rename
/// replaces, and makes the rename durable; then it writes the index, which lists the generation,
/// and makes it durable, with the store's own entry in its parent, which the store's creation
/// does not sync. A payload at the index's next store generation is one whose publication a crash
/// interrupted, which the next publication, or recovery, indexes first, once it has made the
/// rename durable as the publication would have. Each capture completes these publications, and
/// first drops from the index the generations whose payloads a crash lost, which then take none
/// of the store's capacity, before it looks for a generation or counts the store's. A creation
/// that serves a generation that the store holds makes the index, and the store's own entry,
/// durable before it returns it, since the commit that listed it may have been interrupted.
/// Publications leave the older generations, and their leases, untouched.
///
/// A capture that fails before it checks its transaction publishes nothing, and retires the
/// transaction once its VMM has ended. One that a crash of the host interrupts is ended by the
/// next capture of its class, or by [`Store::recover_captures`], which an adapter calls as it
/// starts: each ends the launch that the transaction's sandbox store records, as
/// [`LockedStore::recover`](crate::host::store::LockedStore::recover) ends a sandbox's, and then
/// publishes the transaction if the capture had checked it, once it has synced the transaction
/// again, and retires it otherwise, once it has moved the VMM's log and outcome report to the
/// slot.
///
/// # Leases, tombstones, and retirement
///
/// A clone that restores a generation leases it first, so that the generation stays in the store
/// until the clone's VM has exited. [`Store::select`] selects the generation of a key from which
/// a clone restores: the latest that is neither quarantined nor ineligible at the host's time,
/// as [`Generation::eligibility`] decides it. [`Store::acquire`] leases a generation in one
/// transaction, under the store's lock and then the sandbox's: it checks the generation's
/// eligibility, and writes the record of the lease in the store and the clone binding,
/// [`CLONE_BINDING_NAME`](crate::host::store::CLONE_BINDING_NAME), beside the sandbox's launch
/// journal, before any VMM of the clone is spawned. It returns the lease with the sandbox's lock,
/// under which the caller launches the clone, and holds no lock of the store for the VM's
/// lifetime. The lease reserves one launch of the clone, [`Lease::launch`], which only that lock
/// begins, with
/// [`LockedStore::begin_launch`](crate::host::store::LockedStore::begin_launch), and the sandbox
/// launches nothing else while the lease lasts; [the supervisor](crate::host::supervisor#clones)
/// describes the launch of a clone, its crashes, and the order of the locks that it takes. A lease
/// never expires: it lasts until [`Store::release`] ends it, which it does only
/// once the sandbox records no VM and no launch, and so once its VM has exited, as its store
/// verifies through the VM's recorded process, also after a reattach. A caller that reads a
/// generation's payload, as a restore does, holds a lease of it.
///
/// On Linux, a payload holds the control socket of the VMM of each clone that leases it,
/// `<launch>.sock`, named after the launch that the lease reserves, beside the snapshot that the
/// clone restores, and of no clone whose lease has ended: the release makes the socket's absence
/// durable before it ends the lease. So a payload holds entries other than its capture's while
/// its generation has leases, which nothing that reads an indexed payload may refuse, and a
/// retirement deletes the payload with any socket that a crash, or a removal of a sandbox that
/// never released its lease, left in it.
///
/// A tombstone, which [`Store::tombstone`] writes, quarantines a generation: it takes no new
/// lease, and selection skips it, while the leases that it granted keep working.
/// [`Store::retire`] tombstones a generation, and deletes it once its leases have drained: its
/// payload, then its entry in the index, and then its tombstone, so that an interrupted
/// retirement leaves a quarantined generation, which retiring it again deletes. A release, or a
/// removal of a sandbox, that is interrupted leaves the record of a lease that no clone binding
/// names, which a retirement removes.
///
/// These transactions make each step durable before the next, so that their order holds through
/// a crash of the host too, even when a sandbox and the store are on different filesystems: an
/// acquisition makes the record of a lease durable before it writes the clone binding, and the
/// binding before it returns, and a release, or a retirement that finds a binding gone, makes the
/// binding's absence durable before it removes the record, as a
/// [removal](crate::host::store#removal) of a sandbox does before the sandbox goes, so that no
/// binding outlasts the record that keeps its generation; a retirement makes the tombstone durable
/// before it deletes anything, and each deletion before the next. On Windows, which has no
/// portable way to flush a directory, a rename or a removal is as durable as the filesystem makes
/// it, as OpenVMM's publication of a snapshot assumes.
///
/// # Privacy
///
/// The store's directories are private to the current user, the orchestrator's account, so that
/// no one else may write its records and payloads: each belongs to the user, and no one else may
/// access it. On Unix, a directory grants the group and others no permission, and those that the
/// store creates have mode 0700. On Windows, a directory that the store creates allows the user
/// alone, through a protected access control list that its files inherit; one that it did not
/// create may also allow SYSTEM and the Administrators group, as one under the user's profile
/// does, and must pass entries on to the files created in it. A snapshot's directory, which
/// OpenVMM creates, need not: OpenVMM gives it, and each file that it publishes in it, a protected
/// list of their own, which allows SYSTEM and the owner, and the directory's list already stops
/// anyone else from creating a file in it. The store refuses a record whose
/// own list allows anyone else. A directory or a file of the store that is a link, or on Windows
/// any other reparse point, is refused, and so is a record that is not a regular file. The store
/// names its files by their paths, so the directories above it must be ones that only the user,
/// or an administrator, may change.
///
/// # Example
///
/// ```no_run
/// use nvx::host::template::Store;
///
/// # fn example() -> nvx::Result<()> {
/// let store = Store::create("/var/lib/example/templates")?;
/// for generation in store.index()?.generations() {
///     let key = store.read_key(generation)?;
///     println!(
///         "generation {} of key {} on {}",
///         generation.generation(),
///         generation.key(),
///         key.hypervisor()
///     );
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Store {
    /// The store's directory, as an absolute path.
    dir: PathBuf,
    /// The store's identity.
    id: StoreId,
}

#[cfg(test)]
impl Store {
    /// Publishes a generation of a template, whose fixed time contract names a host and boot of
    /// its own, for the tests of other modules, and returns it.
    pub(crate) fn publish_for_tests(&self) -> Generation {
        tests::publish(
            self,
            &super::test_support::ManifestSpec::template(),
            tests::storage(1),
        )
    }

    /// Publishes a capture of `class` whose manifest `spec` encodes, as
    /// [`publish_for_tests`](Self::publish_for_tests) does, for the tests of the other modules of
    /// templates, and returns it.
    #[cfg(any(target_os = "linux", windows))]
    pub(super) fn publish_spec_for_tests(
        &self,
        class: &super::CaptureClass,
        spec: &super::test_support::ManifestSpec,
    ) -> Generation {
        tests::publish_with(self, class, spec, tests::storage(1))
    }
}

impl Store {
    /// Creates the directory `dir` if needed, whose parent must exist, private to the current
    /// user, and a store in it, or opens the store that it holds.
    ///
    /// A directory that holds no store must hold nothing but what an interrupted creation leaves:
    /// one with any other entry fails with [`ErrorCode::ForeignFiles`]. The creation writes the
    /// store's identity record last, so a store whose creation is interrupted is none, and a
    /// later creation replaces it. Creations of one store take its lock, so that they make one
    /// store between them. An existing directory must already be private, as [`open`](Self::open)
    /// requires: one that others could access is refused rather than restricted, since they could
    /// hold its files open.
    ///
    /// A relative `dir` is resolved against the current directory. Fails with
    /// [`ErrorKind::NotFound`] if the parent does not exist.
    pub fn create(dir: impl Into<PathBuf>) -> Result<Self> {
        Self::create_racing(absolute(dir.into())?, || {})
    }

    /// Creates the store as [`create`](Self::create) does, in the absolute directory `dir`, and
    /// runs `raced` once it has found no store there, before it checks the directory's entries,
    /// where a concurrent creation may make one.
    fn create_racing(dir: PathBuf, raced: impl FnOnce()) -> Result<Self> {
        let problem = platform::create_owner_only_dir(&dir).map_err(store_error(format!(
            "cannot create the template store {}",
            dir.display()
        )))?;
        refuse_problem(&dir, "the template store", problem)?;
        // A directory of anything else is refused before its lock file is created, unless a
        // concurrent creation made a store in it meanwhile, and again under the lock.
        if read_identity(&dir)?.is_none() {
            raced();
            if let Err(error) = refuse_foreign_entries(&dir)
                && read_identity(&dir)?.is_none()
            {
                return Err(error);
            }
        }
        let lock = take_lock(&dir, None)?;
        if let Some(id) = read_identity(&dir)? {
            let store = Self { dir, id };
            store.check_generations_dir()?;
            return Ok(store);
        }
        refuse_foreign_entries(&dir)?;
        let generations = dir.join(GENERATIONS_NAME);
        let problem = platform::create_owner_only_dir(&generations).map_err(store_error(
            format!("cannot create the template store {}", dir.display()),
        ))?;
        refuse_problem(
            &generations,
            "the generations of the template store",
            problem,
        )?;
        let store = Self {
            dir,
            id: StoreId::from_bytes(random_id("a template store's identity")?),
        };
        store.write_index(&Index {
            next: FIRST_GENERATION,
            generations: Vec::new(),
        })?;
        write_record(
            &store.dir.join(STORE_RECORD_NAME),
            &StoreRecord {
                format: STORE_FORMAT,
                store: store.id.to_string(),
            },
            "the identity record of the template store",
        )?;
        drop(lock);
        Ok(store)
    }

    /// Opens the store in the existing directory `dir`, which a relative path names from the
    /// current directory, and which must be private to the current user, as
    /// [`create`](Self::create) makes it.
    ///
    /// Fails with [`ErrorKind::NotFound`] if `dir` does not exist, or holds no store, and with
    /// [`ErrorCode::FormatUnsupported`] if it holds a store of another format.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = absolute(dir.into())?;
        let problem = platform::check_owner_only_dir(&dir).map_err(store_error(format!(
            "cannot open the template store {}",
            dir.display()
        )))?;
        refuse_problem(&dir, "the template store", problem)?;
        let Some(id) = read_identity(&dir)? else {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!("the directory {} holds no template store", dir.display()),
            ));
        };
        let store = Self { dir, id };
        store.check_generations_dir()?;
        Ok(store)
    }

    /// Returns the store's directory, as an absolute path.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Returns the store's identity.
    pub fn id(&self) -> StoreId {
        self.id
    }

    /// Reads the store's index.
    ///
    /// A read that holds no lock sees one whole index, as the latest transaction left it.
    /// Fails with [`ErrorCode::FormatUnsupported`] for an index of another format, and with
    /// [`ErrorKind::Io`] for one that cannot be read, is malformed, or belongs to another store.
    pub fn index(&self) -> Result<Index> {
        let path = self.dir.join(INDEX_NAME);
        let description = "index of the template store";
        let Some(record) = read_record::<IndexRecord>(&path, description, MAX_INDEX_LEN)? else {
            return Err(Error::io(format!(
                "the template store {} has no index",
                self.dir.display()
            )));
        };
        let malformed = |problem: String| {
            Error::io(format!(
                "the {description} {} is malformed: {problem}",
                path.display()
            ))
        };
        if record.store != self.id.to_string() {
            return Err(Error::io(format!(
                "the {description} {} belongs to the template store {}, not to this one, {}",
                path.display(),
                record.store,
                self.id
            )));
        }
        record.into_index().map_err(malformed)
    }

    /// Returns the path of the payload of the store generation `generation`.
    pub fn payload_dir(&self, generation: StoreGeneration) -> PathBuf {
        self.dir.join(GENERATIONS_NAME).join(generation.to_string())
    }

    /// Checks the payload of `generation`, and returns the path of its snapshot, which OpenVMM
    /// restores.
    ///
    /// The directories of the generations, of the payload, and of the snapshot must each be
    /// private to the current user, and none of them a link, or on Windows any other reparse
    /// point. The snapshot's directory is the one that OpenVMM published, which need not pass
    /// access control entries on to the files created in it; see [Privacy](Store#privacy). Fails
    /// with [`ErrorKind::NotFound`] if the payload or its snapshot is missing.
    pub fn snapshot_dir(&self, generation: &Generation) -> Result<PathBuf> {
        let payload = self.payload_dir(generation.generation);
        let snapshot = payload.join(SNAPSHOT_NAME);
        self.check_generations_dir()?;
        for (path, what, check) in [
            (
                &payload,
                "the payload of the generation",
                platform::check_owner_only_dir as fn(&Path) -> io::Result<Option<String>>,
            ),
            (
                &snapshot,
                "the snapshot of the generation",
                platform::check_snapshot_dir,
            ),
        ] {
            let problem = check(path).map_err(store_error(format!(
                "cannot open {what} {}",
                path.display()
            )))?;
            refuse_problem(path, what, problem)?;
        }
        Ok(snapshot)
    }

    /// Reads the compatibility key of `generation` from its payload.
    ///
    /// The key is read as it was written when the generation was indexed, without computing its
    /// identity again. Fails with [`ErrorKind::NotFound`] if the payload or its key is missing,
    /// and as [`TemplateKey::from_bytes`] does for a key that is not the canonical encoding of
    /// one.
    pub fn read_key(&self, generation: &Generation) -> Result<TemplateKey> {
        let payload = self.payload_dir(generation.generation);
        self.check_generations_dir()?;
        let what = "the payload of the generation";
        let problem = platform::check_owner_only_dir(&payload).map_err(store_error(format!(
            "cannot open {what} {}",
            payload.display()
        )))?;
        refuse_problem(&payload, what, problem)?;
        let path = payload.join(KEY_NAME);
        let description = "template key";
        let bytes =
            read_private(&path, description, TemplateKey::MAX_LEN as u64)?.ok_or_else(|| {
                Error::new(
                    ErrorKind::NotFound,
                    format!("the {description} {} is missing", path.display()),
                )
            })?;
        TemplateKey::from_bytes(&bytes).map_err(|error| {
            Error::new(
                error.code(),
                format!("cannot read {}: {}", path.display(), error.message()),
            )
        })
    }

    /// Checks that the directory of the generations is private to the current user, and not a
    /// link, as every read of a generation does, so that a clone's plan tells a store whose
    /// directory fails it, the host's, from a generation that is malformed.
    pub(super) fn check_generations_dir(&self) -> Result<()> {
        let path = self.dir.join(GENERATIONS_NAME);
        let what = "the generations of the template store";
        let problem = platform::check_owner_only_dir(&path).map_err(store_error(format!(
            "cannot open {what} {}",
            path.display()
        )))?;
        refuse_problem(&path, what, problem)
    }

    /// Atomically replaces the store's index with `index`.
    fn write_index(&self, index: &Index) -> Result<()> {
        write_record(
            &self.dir.join(INDEX_NAME),
            &IndexRecord::new(self.id, index),
            "the index of the template store",
        )
    }
}

/// Returns `dir` as an absolute path, which a relative one names from the current directory,
/// without trailing separators: Linux follows a link at a name that a separator ends, even
/// without following links at the final name, so the checks of the store's directory refuse a
/// link at its own name only without them.
fn absolute(dir: PathBuf) -> Result<PathBuf> {
    std::path::absolute(&dir)
        .map(|absolute| absolute.components().collect())
        .map_err(|error| {
            Error::io(format!(
                "cannot resolve the template store {}",
                dir.display()
            ))
            .with_source(error)
        })
}

/// Fails unless `problem`, why the directory `path`, which `what` names, is not private to the
/// current user, is `None`.
fn refuse_problem(path: &Path, what: &str, problem: Option<String>) -> Result<()> {
    match problem {
        None => Ok(()),
        Some(problem) => Err(Error::io(format!("{what} {} {problem}", path.display()))),
    }
}

/// Returns the names of the entries of the directory `dir`.
fn entries(dir: &Path) -> Result<Vec<String>> {
    let failed = || store_error(format!("cannot list {}", dir.display()));
    fs::read_dir(dir)
        .map_err(failed())?
        .map(|entry| {
            Ok(entry
                .map_err(failed())?
                .file_name()
                .to_string_lossy()
                .into_owned())
        })
        .collect()
}

/// Returns whether `name` is an entry that a creation of a store makes before its identity record:
/// the index, its lock, the directory of the generations, and the temporary files that an
/// interrupted replacement of a record leaves, `.NAME.NUMBER.tmp`.
fn is_creation_leftover(name: &str) -> bool {
    [INDEX_NAME, INDEX_LOCK_NAME, GENERATIONS_NAME].contains(&name)
        || name
            .strip_prefix('.')
            .and_then(|name| name.strip_suffix(".tmp"))
            .and_then(|name| name.rsplit_once('.'))
            .is_some_and(|(file, number)| {
                [STORE_RECORD_NAME, INDEX_NAME].contains(&file)
                    && !number.is_empty()
                    && number.bytes().all(|byte| byte.is_ascii_digit())
            })
}

/// Fails with [`ErrorCode::ForeignFiles`] if the directory `dir`, which holds no store, holds
/// anything but what an interrupted creation of one leaves: any other entry, or any in the
/// directory of the generations, which must be private to the current user, if it exists.
fn refuse_foreign_entries(dir: &Path) -> Result<()> {
    let mut foreign = entries(dir)?
        .into_iter()
        .filter(|name| !is_creation_leftover(name))
        .collect::<Vec<_>>();
    let generations = dir.join(GENERATIONS_NAME);
    let what = "the generations of the template store";
    match platform::check_owner_only_dir(&generations) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        checked => {
            let problem = checked.map_err(store_error(format!(
                "cannot open {what} {}",
                generations.display()
            )))?;
            refuse_problem(&generations, what, problem)?;
            foreign.extend(
                entries(&generations)?
                    .into_iter()
                    .map(|name| format!("{GENERATIONS_NAME}/{name}")),
            );
        }
    }
    if foreign.is_empty() {
        return Ok(());
    }
    foreign.sort();
    Err(Error::new(
        ErrorCode::ForeignFiles,
        format!(
            "the directory {} holds no template store, but entries that are not a store's: {}",
            dir.display(),
            foreign.join(", ")
        ),
    ))
}

/// Opens the lock file of the store in `dir`, which it creates if needed, takes its lock, waiting
/// for it until `deadline`, or for as long as it takes without one, and checks that the path
/// still names the locked file.
///
/// The store never removes or replaces its lock file, so one that its path no longer names once
/// it is locked was removed or replaced by someone else, and its lock serializes nothing: the
/// lock is then taken again, after a pause, until the deadline.
fn take_lock(dir: &Path, deadline: Option<Instant>) -> Result<File> {
    let path = dir.join(INDEX_LOCK_NAME);
    let failed = |error| store_error(format!("cannot lock {}", path.display()))(error);
    let busy = || {
        Error::new(
            ErrorKind::TimedOut,
            format!(
                "another transaction of the template store {} is still in progress",
                dir.display()
            ),
        )
    };
    loop {
        let file = open_lock(&path).map_err(failed)?;
        if let Some(problem) = platform::private_file_problem(&file).map_err(failed)? {
            return Err(Error::io(format!(
                "the lock of the template store {} {problem}",
                path.display()
            )));
        }
        match deadline {
            None => file.lock().map_err(failed)?,
            Some(deadline) => loop {
                match file.try_lock() {
                    Ok(()) => break,
                    Err(TryLockError::WouldBlock) => {
                        let left = deadline.saturating_duration_since(Instant::now());
                        if left.is_zero() {
                            return Err(busy());
                        }
                        thread::sleep(left.min(LOCK_RETRY_INTERVAL));
                    }
                    Err(TryLockError::Error(error)) => return Err(failed(error)),
                }
            },
        }
        let locked = platform::opened_file_id(&file).map_err(failed)?;
        match platform::file_id(&path) {
            Ok(current) if current == locked => return Ok(file),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) if cfg!(windows) && error.kind() == io::ErrorKind::PermissionDenied => {}
            Err(error) => return Err(failed(error)),
        }
        drop(file);
        let left = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
        if left.is_some_and(|left| left.is_zero()) {
            return Err(busy());
        }
        thread::sleep(left.map_or(LOCK_RETRY_INTERVAL, |left| left.min(LOCK_RETRY_INTERVAL)));
    }
}

/// Atomically replaces the store's record at `path`, its `description`, with `record`, as pretty
/// JSON and a newline.
fn write_record<T: Serialize>(path: &Path, record: &T, description: &str) -> Result<()> {
    let mut text = serde_json::to_vec_pretty(record)
        .map_err(|error| Error::other(format!("cannot encode {description}")).with_source(error))?;
    text.push(b'\n');
    write_private(path, &text)
}

/// The index of a store, as JSON.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IndexRecord {
    /// [`STORE_FORMAT`].
    format: u32,
    /// The identity of the store.
    store: String,
    /// The store generation that the next publication takes.
    next_generation: u64,
    /// The generations, in the order of their store generations.
    generations: Vec<GenerationRecord>,
}

impl IndexRecord {
    /// Returns the record of `index`, of the store `store`.
    fn new(store: StoreId, index: &Index) -> Self {
        Self {
            format: STORE_FORMAT,
            store: store.to_string(),
            next_generation: index.next.get().get(),
            generations: index
                .generations
                .iter()
                .map(GenerationRecord::new)
                .collect(),
        }
    }

    /// Checks the record, and returns its index, or says why it is malformed.
    fn into_index(self) -> std::result::Result<Index, String> {
        let next = NonZeroU64::new(self.next_generation)
            .map(StoreGeneration::new)
            .ok_or("its next store generation is 0")?;
        if self.generations.len() > MAX_GENERATIONS {
            return Err(format!(
                "it lists {} generations, more than the {MAX_GENERATIONS} that a store holds",
                self.generations.len()
            ));
        }
        let mut generations = Vec::with_capacity(self.generations.len());
        for record in self.generations {
            let generation = record.into_generation()?;
            if generations
                .last()
                .is_some_and(|last: &Generation| last.generation >= generation.generation)
            {
                return Err(format!(
                    "it lists store generation {} out of order, or twice",
                    generation.generation
                ));
            }
            if generation.generation >= next {
                return Err(format!(
                    "it lists store generation {}, which its next store generation, {next}, does \
                     not follow",
                    generation.generation
                ));
            }
            generations.push(generation);
        }
        Ok(Index { next, generations })
    }
}

/// A generation in the index of a store, as JSON.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GenerationRecord {
    /// The store generation.
    generation: u64,
    /// The identity of the key.
    key: String,
    /// The identity of the capture class.
    class: String,
    /// The storage generation of the capture.
    storage_generation: String,
    /// When OpenVMM created the snapshot.
    created_at: TimestampRecord,
    /// The time contract of the capture.
    time: TimeRecord,
    /// The length of the saved state, in bytes.
    state_len: u64,
    /// The length of the RAM, in bytes.
    memory_len: u64,
}

impl GenerationRecord {
    /// Returns the record of `generation`.
    fn new(generation: &Generation) -> Self {
        let created_at = generation
            .created_at
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        Self {
            generation: generation.generation.get().get(),
            key: generation.key.to_string(),
            class: generation.class.to_string(),
            storage_generation: generation.storage.to_string(),
            created_at: TimestampRecord {
                seconds: created_at.as_secs(),
                nanos: created_at.subsec_nanos(),
            },
            time: TimeRecord::new(&generation.time),
            state_len: generation.state_len,
            memory_len: generation.memory_len,
        }
    }

    /// Checks the record, and returns its generation, or says why it is malformed.
    fn into_generation(self) -> std::result::Result<Generation, String> {
        let generation = NonZeroU64::new(self.generation)
            .map(StoreGeneration::new)
            .ok_or("it lists store generation 0, which no generation has")?;
        let field = |what: &str, error: Error| {
            format!(
                "the {what} of store generation {generation} is invalid: {}",
                error.message()
            )
        };
        let created_at = (self.created_at.nanos < 1_000_000_000)
            .then(|| {
                SystemTime::UNIX_EPOCH.checked_add(Duration::new(
                    self.created_at.seconds,
                    self.created_at.nanos,
                ))
            })
            .flatten()
            .ok_or_else(|| {
                format!("the creation time of store generation {generation} is invalid")
            })?;
        Ok(Generation {
            generation,
            key: self.key.parse().map_err(|error| field("key", error))?,
            class: self.class.parse().map_err(|error| field("class", error))?,
            storage: self
                .storage_generation
                .parse()
                .map_err(|error| field("storage generation", error))?,
            created_at,
            time: self.time.into_contract().map_err(|problem| {
                format!("the time contract of store generation {generation} {problem}")
            })?,
            state_len: self.state_len,
            memory_len: self.memory_len,
        })
    }
}

/// A time, as JSON: seconds and nanoseconds since the Unix epoch.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TimestampRecord {
    /// Whole seconds.
    seconds: u64,
    /// Nanoseconds within the second.
    nanos: u32,
}

/// The time contract of a capture, as JSON.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TimeRecord {
    /// [`TimeContract::tsc_frequency_hz`].
    tsc_frequency_hz: u64,
    /// [`TimeContract::tsc_tolerance_ppm`].
    tsc_tolerance_ppm: u32,
    /// [`TimeContract::apic_frequency_hz`].
    apic_frequency_hz: u64,
    /// [`TimeContract::capture_tsc`].
    capture_tsc: u64,
    /// [`TimeContract::capture_utc_ns`].
    capture_utc_ns: u64,
    /// [`TimeContract::capture_monotonic_ns`].
    capture_monotonic_ns: u64,
    /// [`TimeContract::host_clock`], as a manifest spells it.
    host_clock: String,
    /// [`TimeContract::host_id`], as 32 hexadecimal digits.
    host_id: String,
    /// [`TimeContract::boot_id`].
    boot_id: BootId,
    /// [`TimeContract::lineage`].
    lineage: u32,
    /// [`TimeContract::cut_to_anchor_ns`].
    cut_to_anchor_ns: Option<u64>,
}

impl TimeRecord {
    /// Returns the record of `time`.
    fn new(time: &TimeContract) -> Self {
        Self {
            tsc_frequency_hz: time.tsc_frequency_hz,
            tsc_tolerance_ppm: time.tsc_tolerance_ppm,
            apic_frequency_hz: time.apic_frequency_hz,
            capture_tsc: time.capture_tsc,
            capture_utc_ns: time.capture_utc_ns,
            capture_monotonic_ns: time.capture_monotonic_ns,
            host_clock: time.host_clock.as_str().to_owned(),
            host_id: hex(&time.host_id),
            boot_id: time.boot_id,
            lineage: time.lineage,
            cut_to_anchor_ns: time.cut_to_anchor_ns,
        }
    }

    /// Checks the record, and returns its time contract, or says why it is invalid.
    fn into_contract(self) -> std::result::Result<TimeContract, String> {
        Ok(TimeContract {
            tsc_frequency_hz: self.tsc_frequency_hz,
            tsc_tolerance_ppm: self.tsc_tolerance_ppm,
            apic_frequency_hz: self.apic_frequency_hz,
            capture_tsc: self.capture_tsc,
            capture_utc_ns: self.capture_utc_ns,
            capture_monotonic_ns: self.capture_monotonic_ns,
            host_clock: HostClock::parse(&self.host_clock)
                .ok_or_else(|| format!("has the unknown host clock {:?}", self.host_clock))?,
            host_id: parse_hex_id(&self.host_id)
                .ok_or_else(|| format!("has the invalid host identity {:?}", self.host_id))?,
            boot_id: self.boot_id,
            lineage: self.lineage,
            cut_to_anchor_ns: self.cut_to_anchor_ns,
        })
    }
}
