//! The leases of a store's generations, the bindings of clones to them, the tombstones that
//! quarantine generations, and the retirement of generations whose leases have drained; see
//! [the store's documentation](Store#leases-tombstones-and-retirement).

#[cfg(test)]
mod tests;

use std::fmt;
use std::fs;
use std::io;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use super::{
    GENERATIONS_NAME, Generation, ID_LEN, STORE_FORMAT, Store, StoreId, entries, hex, random_id,
    read_record, refuse_problem, store_error, take_lock, write_record,
};
use crate::activation::{Identity, StoreGeneration, TemplateReference};
use crate::error::{Error, ErrorCode, ErrorKind, Result};
use crate::host::filesystem;
use crate::host::platform::{self, parse_hex_id};
use crate::host::store::files::{remove_if_present, sync_dir};
use crate::host::store::{CLONE_BINDING_NAME, CallerFiles, LockedStore, SandboxStore};
use crate::host::template::eligibility::HostTime;
use crate::host::template::files::was_removed;
use crate::host::template::key::KeyIdentity;

/// Name of the directory of the records of the leases that a store granted.
pub const LEASES_NAME: &str = "leases";
/// Name of the directory of the tombstones of a store's generations.
pub const TOMBSTONES_NAME: &str = "tombstones";

/// Syncs a directory, as [`sync_dir`] does. The transactions of leases take it as a parameter, so
/// that tests observe the order of their syncs, and fail them.
type SyncDir<'a> = &'a dyn Fn(&Path) -> Result<()>;

/// The directories of leases and tombstones whose entries in their stores this process made
/// durable, by the directory and the identity of the store, and the name of the directory. A store
/// creates each once, and never removes it, so one sync of the store's directory, and of its
/// parent, once a process sees one, keeps its entry, and the store's, durable, even if the
/// process that created them did not finish its own syncs.
static DURABLE_RECORD_DIRS: Mutex<Vec<DurableRecordDir>> = Mutex::new(Vec::new());

/// An entry of [`DURABLE_RECORD_DIRS`]: the directory and the identity of a store, and the name
/// of one of its directories.
type DurableRecordDir = (PathBuf, StoreId, &'static str);

/// Returns [`DURABLE_RECORD_DIRS`], locked.
fn durable_record_dirs() -> MutexGuard<'static, Vec<DurableRecordDir>> {
    DURABLE_RECORD_DIRS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Longest record of a lease, of a clone binding, or of a tombstone, in bytes: more than a record
/// that names the longest path of a sandbox takes.
const MAX_LEASE_RECORD_LEN: u64 = 256 * 1024;

/// The directory of a store's leases, in messages.
const LEASES_WHAT: &str = "the directory of the leases of the template store";
/// The directory of a store's tombstones, in messages.
const TOMBSTONES_WHAT: &str = "the directory of the tombstones of the template store";
/// The record of a lease in its store, in messages.
const LEASE_DESCRIPTION: &str = "lease of the template store";
/// The binding of a clone in its sandbox, in messages.
const BINDING_DESCRIPTION: &str = "clone binding";
/// A tombstone, in messages.
const TOMBSTONE_DESCRIPTION: &str = "tombstone of the template store";

/// The identity of a lease: 16 random bytes that its acquisition draws.
///
/// Its text form, which `Display` writes and `FromStr` parses, is 32 lowercase hexadecimal
/// digits; parsing also accepts uppercase ones.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LeaseId([u8; ID_LEN]);

impl LeaseId {
    /// Returns the identity whose bytes are `bytes`.
    pub const fn from_bytes(bytes: [u8; ID_LEN]) -> Self {
        Self(bytes)
    }

    /// Returns the bytes of the identity.
    pub const fn as_bytes(&self) -> &[u8; ID_LEN] {
        &self.0
    }
}

impl fmt::Display for LeaseId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&hex(&self.0))
    }
}

impl fmt::Debug for LeaseId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "LeaseId({self})")
    }
}

impl FromStr for LeaseId {
    type Err = Error;

    /// Parses the text form of a lease's identity, failing with [`ErrorKind::Unsupported`] for
    /// anything else.
    fn from_str(text: &str) -> Result<Self> {
        parse_hex_id(text).map(Self).ok_or_else(|| {
            Error::unsupported(format!(
                "{text:?} is not the identity of a lease, which is 32 hexadecimal digits"
            ))
        })
    }
}

/// A lease of a generation of a template, which keeps the generation in its store for the clone
/// of one sandbox, whose VM restores it.
///
/// The store keeps the record of each lease that it granted, and the sandbox keeps the same
/// record as its clone binding, [`CLONE_BINDING_NAME`], beside its launch journal. A lease names
/// its sandbox by the path of its directory with its links resolved, so that no removal or change
/// of a link, or of a directory that a `..` component passes through, hides the sandbox from a
/// [retirement](Store::retire). A sandbox that holds a lease must not move, and goes only through
/// its store's [removal](LockedStore::remove), which makes the binding's absence durable before
/// the sandbox goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    /// The lease's identity.
    id: LeaseId,
    /// The identity of the store that granted it.
    store: StoreId,
    /// The store generation of the leased generation.
    generation: StoreGeneration,
    /// The identity of the leased generation's key.
    key: KeyIdentity,
    /// The directory of the sandbox that holds the lease, as an absolute path with its links
    /// resolved, which is valid Unicode.
    sandbox: PathBuf,
}

impl Lease {
    /// Reads the clone binding of the sandbox of `sandbox`: the lease that it holds, if it holds
    /// one.
    ///
    /// A read that holds no lock sees one whole binding, as the latest transition left it. Fails
    /// with [`ErrorKind::Io`] for a binding that cannot be read, or is malformed, and with
    /// [`ErrorCode::FormatUnsupported`] for one of another format.
    pub fn of(sandbox: &SandboxStore) -> Result<Option<Self>> {
        read_binding(sandbox.dir())
    }

    /// Returns the lease's identity.
    pub fn id(&self) -> LeaseId {
        self.id
    }

    /// Returns the identity of the store that granted the lease.
    pub fn store(&self) -> StoreId {
        self.store
    }

    /// Returns the store generation of the leased generation.
    pub fn generation(&self) -> StoreGeneration {
        self.generation
    }

    /// Returns the identity of the leased generation's key.
    pub fn key(&self) -> KeyIdentity {
        self.key
    }

    /// Returns the directory of the sandbox that holds the lease, as an absolute path with its
    /// links resolved, which may differ from the path through which the sandbox was leased, and
    /// through which a [release](Store::release) takes the sandbox's lock.
    pub fn sandbox(&self) -> &Path {
        &self.sandbox
    }

    /// Returns the reference by which the activation record of the clone names the template that
    /// it restores: the leased generation's store generation, and its key's identity.
    pub fn template(&self) -> TemplateReference {
        TemplateReference {
            generation: self.generation,
            key: Identity::from(self.key),
        }
    }
}

/// A lease that [`Store::acquire`] granted, with the lock of the sandbox that holds it.
#[derive(Debug)]
pub struct Acquired {
    /// The lease.
    pub lease: Lease,
    /// The lock of the sandbox, which names it by the lease's path, under which the caller launches
    /// its clone. Dropping it releases the lock, and keeps the lease.
    pub locked: LockedStore,
}

/// What [`Store::retire`] found of a generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Retirement {
    /// The generation is gone, with its payload, its entry in the index, and its tombstone, or was
    /// gone already.
    Retired,
    /// A tombstone quarantines the generation, which keeps its payload and its entry in the index
    /// for its leases, which this lists in the order of their identities, until their clones
    /// release them.
    Draining(Vec<Lease>),
}

impl Store {
    /// Selects the generation of the key `key` from which a clone restores at the host's time
    /// `now`: the latest that the store indexes, that no tombstone quarantines, and that is
    /// eligible at `now`, as [`Generation::eligibility`] decides it, if one is.
    ///
    /// The selection reads the index and the tombstones without the store's lock, as
    /// [`index`](Self::index) does, so a selected generation may be tombstoned or retired before
    /// the caller leases it, which [`acquire`](Self::acquire) refuses.
    pub fn select(&self, key: KeyIdentity, now: &HostTime) -> Result<Option<Generation>> {
        let index = self.index()?;
        let tombstoned = self.tombstones()?;
        Ok(index
            .generations
            .iter()
            .rev()
            .find(|generation| {
                generation.key == key
                    && !tombstoned.contains(&generation.generation)
                    && generation.eligibility(now).is_eligible()
            })
            .cloned())
    }

    /// Leases the generation `generation` to the clone of the sandbox in the directory `sandbox`,
    /// whose caller keeps `files` there, at the host's time `now`, as [`HostTime::now`] samples
    /// it, waiting for the store's lock and the sandbox's until `deadline`, or for as long as it
    /// takes without one.
    ///
    /// In one transaction, under the store's lock and then the sandbox's
    /// [lifecycle lock](crate::host::store#lifecycle-locks), it checks that the store indexes the
    /// generation, that no tombstone quarantines it, that it is eligible at `now`, as
    /// [`Generation::eligibility`] decides it, and that the sandbox is provisioned, records no VM
    /// and no launch, and holds no lease; and it writes the lease's record in the store, and then
    /// the same record as the sandbox's clone binding, beside its launch journal, each durable
    /// before the next step, so that no binding outlasts, through a crash of the host, the record
    /// that keeps its generation in the store. It resolves the links of the sandbox's path before
    /// it takes the sandbox's lock, and the lease, the binding, and the lock that it returns all
    /// name the sandbox by that path, as [`Lease::sandbox`] returns it, which no later change of a
    /// link redirects to another directory. It returns the lease, with the sandbox's lock, under
    /// which the caller launches the clone, and releases the store's lock.
    ///
    /// Fails, and leases nothing:
    ///
    /// - with [`ErrorKind::NotFound`] if the store does not index the generation;
    /// - with [`ErrorCode::TemplateTombstoned`] if a tombstone quarantines it;
    /// - with [`ErrorCode::TemplateIneligible`] if it is not eligible at `now`;
    /// - with [`ErrorCode::SandboxNotProvisioned`] if the directory does not exist, or holds no
    ///   configuration record of the caller, as `files` declares it;
    /// - with [`ErrorCode::SandboxRunning`] if the sandbox records a VM, or a launch, which may
    ///   still run;
    /// - with [`ErrorCode::SandboxLeased`] if the sandbox holds a lease already;
    /// - with [`ErrorKind::Unsupported`] if the sandbox's path, with its links resolved, is not
    ///   valid Unicode, which its record cannot hold, or, on Windows, is a network path, or one
    ///   that names the directory only in its verbatim form;
    /// - with [`ErrorKind::TimedOut`] if the deadline passes before it takes the store's lock, and
    ///   as [`SandboxStore::lock_existing`] fails before it takes the sandbox's.
    ///
    /// A failure to write the binding, or to make it durable, leases nothing either, unless the
    /// binding's removal cannot be made durable: the lease's record then stays, and the error says
    /// that the sandbox may hold the lease still, which a [release](Self::release) ends.
    pub fn acquire(
        &self,
        generation: StoreGeneration,
        sandbox: impl Into<PathBuf>,
        files: &CallerFiles,
        now: &HostTime,
        deadline: Option<Instant>,
    ) -> Result<Acquired> {
        self.acquire_with(generation, sandbox.into(), files, now, deadline, &sync_dir)
    }

    /// Leases a generation as [`acquire`](Self::acquire) does, making its records durable with
    /// `sync`.
    fn acquire_with(
        &self,
        generation: StoreGeneration,
        sandbox: PathBuf,
        files: &CallerFiles,
        now: &HostTime,
        deadline: Option<Instant>,
        sync: SyncDir<'_>,
    ) -> Result<Acquired> {
        let lock = take_lock(&self.dir, deadline)?;
        let index = self.index()?;
        let indexed = index
            .get(generation)
            .ok_or_else(|| self.not_indexed(generation))?;
        if self.is_tombstoned(generation)? {
            return Err(Error::new(
                ErrorCode::TemplateTombstoned,
                format!(
                    "a tombstone quarantines the generation {generation} of the template store \
                     {}, which takes no new clones",
                    self.dir.display()
                ),
            ));
        }
        let eligibility = indexed.eligibility(now);
        if !eligibility.is_eligible() {
            return Err(Error::new(
                ErrorCode::TemplateIneligible,
                format!(
                    "the generation {generation} of the template store {} is {eligibility}",
                    self.dir.display()
                ),
            ));
        }
        let not_provisioned = || {
            Error::new(
                ErrorCode::SandboxNotProvisioned,
                format!("no sandbox is provisioned in {}", sandbox.display()),
            )
        };
        // The lease, the binding, and the lock name the sandbox by its path with its links
        // resolved, which no later change of a link redirects to another directory.
        let resolved = match filesystem::canonicalize(&sandbox) {
            Ok(resolved) => resolved,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(not_provisioned().with_source(error));
            }
            Err(error) => {
                let message = format!(
                    "cannot resolve the path of the sandbox in {}",
                    sandbox.display()
                );
                return Err(if error.kind() == io::ErrorKind::Unsupported {
                    Error::unsupported(message).with_source(error)
                } else {
                    store_error(message)(error)
                });
            }
        };
        let locked = match SandboxStore::lock_existing(&resolved, files, deadline) {
            Ok(locked) => locked,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Err(not_provisioned().with_source(error));
            }
            Err(error) => return Err(error),
        };
        if let Some(config) = files.config() {
            let path = locked.dir().join(config);
            match fs::symlink_metadata(&path) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Err(not_provisioned());
                }
                Err(error) => {
                    return Err(store_error(format!("cannot examine {}", path.display()))(
                        error,
                    ));
                }
            }
        }
        if locked.journal()?.is_some() || locked.runtime()?.is_some() {
            return Err(may_run(locked.dir()));
        }
        if let Some(held) = read_binding(locked.dir())? {
            return Err(Error::new(
                ErrorCode::SandboxLeased,
                format!(
                    "the sandbox in {} holds the lease {} of the generation {} of the template \
                     store {} already",
                    locked.dir().display(),
                    held.id,
                    held.generation,
                    held.store
                ),
            ));
        }
        if locked.dir().to_str().is_none() {
            return Err(Error::unsupported(format!(
                "the path of the sandbox {} is not valid Unicode, which the record of a lease \
                 cannot hold",
                locked.dir().display()
            )));
        }
        let lease = Lease {
            id: LeaseId(random_id("a lease's identity")?),
            store: self.id,
            generation,
            key: indexed.key,
            sandbox: locked.dir().to_owned(),
        };
        let record = LeaseRecord::new(&lease);
        self.create_records_dir(LEASES_NAME, LEASES_WHAT, sync)?;
        let path = self.lease_path(lease.id);
        write_record(&path, &record, "the lease of the template store")?;
        // A retirement deletes a generation that no record of the store leases, so the record
        // must be durable before a binding names it.
        if let Err(error) = sync(&self.dir.join(LEASES_NAME)) {
            let _ = remove_if_present(&path);
            return Err(error);
        }
        if let Err(error) = bind(locked.dir(), &record, sync) {
            // The record goes only once the binding's removal is durable. Until then, it keeps
            // the generation for a binding that may outlast a crash, and a retirement removes it
            // once no binding names it.
            if unbind(locked.dir(), sync).is_err() {
                return Err(held_still(error, locked.dir(), lease.id));
            }
            let _ = remove_if_present(&path);
            return Err(error);
        }
        drop(lock);
        Ok(Acquired { lease, locked })
    }

    /// Releases the lease that the sandbox of `locked` holds, and returns it, or returns `None` if
    /// the sandbox holds none.
    ///
    /// A lease lasts until its clone's VM has exited: the release refuses with
    /// [`ErrorCode::SandboxRunning`] while the sandbox records a VM or a launch, which may still
    /// run. The sandbox's store clears those records only once it has verified that the VM's
    /// process exited, through its recorded [`ProcessRef`](crate::host::process::ProcessRef), as
    /// a stop or a [`reattach`](crate::host::supervisor::reattach) does. The release removes the
    /// clone binding, and once its removal is durable, the lease's record in the store, so that an
    /// interrupted release leaves a record that no binding names, which a
    /// [retirement](Self::retire) removes, and never a binding without its record. It takes no
    /// lock of the store.
    ///
    /// The caller takes the sandbox's lock through the lease's path, as [`Lease::sandbox`]
    /// returns it, and as [`acquire`](Self::acquire) takes it, which has no links: a link in the
    /// path of the lock could change as the release runs, and make it remove another sandbox's
    /// binding, whose lock it does not hold.
    ///
    /// Fails with [`ErrorKind::Other`] if the sandbox holds a lease of another store, or if the
    /// lock does not name the sandbox by its lease's path, and then removes nothing.
    pub fn release(&self, locked: &mut LockedStore) -> Result<Option<Lease>> {
        self.release_with(locked, &sync_dir)
    }

    /// Releases a lease as [`release`](Self::release) does, making the removal of its binding
    /// durable with `sync`.
    fn release_with(&self, locked: &mut LockedStore, sync: SyncDir<'_>) -> Result<Option<Lease>> {
        if locked.journal()?.is_some() || locked.runtime()?.is_some() {
            return Err(may_run(locked.dir()));
        }
        let Some(lease) = read_binding(locked.dir())? else {
            return Ok(None);
        };
        if lease.store != self.id {
            return Err(Error::other(format!(
                "the sandbox in {} holds a lease of the template store {}, not of this one, {}",
                locked.dir().display(),
                lease.store,
                self.id
            )));
        }
        if locked.dir() != lease.sandbox {
            return Err(Error::other(format!(
                "the lock of the sandbox in {} does not name it by the path of its lease, {}, \
                 through which a release takes it",
                locked.dir().display(),
                lease.sandbox.display()
            )));
        }
        let recorded = self.check_records_dir(LEASES_NAME, LEASES_WHAT)?;
        unbind(locked.dir(), sync)?;
        if recorded {
            remove_lease_record(&self.lease_path(lease.id))?;
        }
        Ok(Some(lease))
    }

    /// Returns the leases of the generation `generation` that the store's records list, in the
    /// order of their identities, including those that an interrupted release or removal of
    /// their sandboxes left, which a [retirement](Self::retire) removes.
    ///
    /// Fails with [`ErrorCode::ForeignFiles`] if the directory of the leases holds entries that
    /// are not the store's.
    pub fn leases(&self, generation: StoreGeneration) -> Result<Vec<Lease>> {
        let mut leases = Vec::new();
        for id in self.records(LEASES_NAME, LEASES_WHAT, lease_name)?.records {
            // A release may remove the record after the listing.
            if let Some(lease) = self.read_lease(id)?
                && lease.generation == generation
            {
                leases.push(lease);
            }
        }
        leases.sort_by_key(|lease| lease.id);
        Ok(leases)
    }

    /// Quarantines the generation `generation` with a tombstone, waiting for the store's lock
    /// until `deadline`, or for as long as it takes without one: the generation takes no new
    /// lease, and [`select`](Self::select) skips it, while its leases keep working. A tombstone
    /// is durable once this returns, and lasts until the generation's
    /// [retirement](Self::retire).
    ///
    /// Tombstoning a generation again does nothing. Fails with [`ErrorKind::NotFound`] if the
    /// store does not index the generation.
    pub fn tombstone(&self, generation: StoreGeneration, deadline: Option<Instant>) -> Result<()> {
        let _lock = take_lock(&self.dir, deadline)?;
        if self.index()?.get(generation).is_none() {
            return Err(self.not_indexed(generation));
        }
        self.write_tombstone(generation, &sync_dir)
    }

    /// Returns the store generations that tombstones quarantine, in order.
    ///
    /// Each tombstone is read and checked, as an acquisition checks it: a tombstone that cannot be
    /// read, or is malformed, fails with [`ErrorKind::Io`], and one of another format with
    /// [`ErrorCode::FormatUnsupported`], rather than quarantining its generation. Fails with
    /// [`ErrorCode::ForeignFiles`] if the directory of the tombstones holds entries that are not
    /// the store's.
    pub fn tombstones(&self) -> Result<Vec<StoreGeneration>> {
        let mut tombstoned = Vec::new();
        for generation in self
            .records(TOMBSTONES_NAME, TOMBSTONES_WHAT, tombstone_name)?
            .records
        {
            // A retirement may remove the tombstone after the listing.
            if self.read_tombstone(generation)? {
                tombstoned.push(generation);
            }
        }
        tombstoned.sort();
        Ok(tombstoned)
    }

    /// Retires the generation `generation`, waiting for the store's lock until `deadline`, or for
    /// as long as it takes without one: it tombstones the generation, and deletes it once its
    /// leases have drained.
    ///
    /// Under the store's lock, it tombstones the generation, and removes the records of its
    /// leases whose sandboxes no longer hold them: a sandbox that is gone, that holds no clone
    /// binding, or one of another lease, as an interrupted release, or a removal of the sandbox,
    /// leaves it. It makes the binding's absence durable first, by syncing the sandbox's
    /// directory, if it is one, and the directory that holds the sandbox's entry, the nearest of
    /// its ancestors that remains, through the links that the sandbox's path names, as a release
    /// that a crash or a failed sync interrupted may not have. A [removal](LockedStore::remove) of
    /// the sandbox made it durable before the sandbox went; for a sandbox that went otherwise,
    /// these syncs make it durable only while the directories that held the sandbox remain. If
    /// any lease remains, the generation keeps its payload and its entry in the index, and the
    /// retirement returns [`Retirement::Draining`] with the leases, whose clones
    /// [release](Self::release) them; a later retirement deletes it. Otherwise it removes the
    /// generation's payload, then its entry in the index, and then its tombstone, each durable
    /// before the next, as the tombstone is before the payload goes, so that a retirement that a
    /// crash interrupts, even one of the host, leaves a tombstoned generation, which retiring it
    /// again deletes, and returns [`Retirement::Retired`]. A generation that the store indexed
    /// once, and no longer indexes, is retired already. Its store generation is never assigned
    /// again.
    ///
    /// It also removes the temporary files that interrupted replacements of the records of leases
    /// and tombstones left, since the store writes those only under its lock.
    ///
    /// Fails with [`ErrorKind::NotFound`] for a store generation that the store never assigned,
    /// and with [`ErrorCode::ForeignFiles`] if the directory of the leases, or of the tombstones,
    /// holds entries that are not the store's. A clone binding that cannot be read, or whose
    /// absence cannot be made durable, fails the retirement, which then deletes no generation.
    pub fn retire(
        &self,
        generation: StoreGeneration,
        deadline: Option<Instant>,
    ) -> Result<Retirement> {
        self.retire_with(generation, deadline, &sync_dir)
    }

    /// Retires a generation as [`retire`](Self::retire) does, making each step durable with `sync`
    /// before the next.
    fn retire_with(
        &self,
        generation: StoreGeneration,
        deadline: Option<Instant>,
        sync: SyncDir<'_>,
    ) -> Result<Retirement> {
        let _lock = take_lock(&self.dir, deadline)?;
        let mut index = self.index()?;
        if index.get(generation).is_none() {
            if generation >= index.next {
                return Err(self.not_indexed(generation));
            }
            // A retirement that was interrupted once it had written the index may not have made
            // it durable, which must be before the tombstone goes.
            sync(&self.dir)?;
            self.remove_tombstone(generation)?;
            return Ok(Retirement::Retired);
        }
        let leases = self.records(LEASES_NAME, LEASES_WHAT, lease_name)?;
        let tombstones = self.records(TOMBSTONES_NAME, TOMBSTONES_WHAT, tombstone_name)?;
        self.write_tombstone(generation, sync)?;
        for temporary in leases.temporaries.iter().chain(&tombstones.temporaries) {
            remove_if_present(temporary)?;
        }
        let mut live = Vec::new();
        for id in leases.records {
            let Some(lease) = self.read_lease(id)? else {
                continue;
            };
            if lease.generation != generation {
                continue;
            }
            if is_bound(&lease)? {
                live.push(lease);
            } else {
                // A release may have removed the binding without making that durable yet, so the
                // binding's absence must be durable before its record goes.
                for dir in binding_dirs(&lease.sandbox)? {
                    sync(dir)?;
                }
                remove_lease_record(&self.lease_path(id))?;
            }
        }
        if !live.is_empty() {
            live.sort_by_key(|lease| lease.id);
            return Ok(Retirement::Draining(live));
        }
        self.remove_payload(generation)?;
        sync(&self.dir.join(GENERATIONS_NAME))?;
        index
            .generations
            .retain(|indexed| indexed.generation != generation);
        self.write_index(&index)?;
        sync(&self.dir)?;
        self.remove_tombstone(generation)?;
        Ok(Retirement::Retired)
    }

    /// Returns the error of the generation `generation`, which the store does not index.
    fn not_indexed(&self, generation: StoreGeneration) -> Error {
        Error::new(
            ErrorKind::NotFound,
            format!(
                "the template store {} holds no generation {generation}",
                self.dir.display()
            ),
        )
    }

    /// Returns the path of the record of the lease `id`.
    fn lease_path(&self, id: LeaseId) -> PathBuf {
        self.dir.join(LEASES_NAME).join(format!("{id}.json"))
    }

    /// Returns the path of the tombstone of the store generation `generation`.
    fn tombstone_path(&self, generation: StoreGeneration) -> PathBuf {
        self.dir
            .join(TOMBSTONES_NAME)
            .join(format!("{generation}.json"))
    }

    /// Creates the store's directory `name`, which `what` names, private to the current user,
    /// unless it exists, under the store's lock, and makes its entry in the store durable with
    /// `sync`, and the store's own entry in its parent: it syncs the store's directory, and then
    /// the store's parent, once it creates the directory, and once it finds one that this process
    /// has not synced yet, whose creation, or the store's, may not have finished its sync. A
    /// binding then never names a record of a store whose own entry a crash of the host could
    /// lose.
    fn create_records_dir(&self, name: &'static str, what: &str, sync: SyncDir<'_>) -> Result<()> {
        let key = (self.dir.clone(), self.id, name);
        if !self.check_records_dir(name, what)? {
            let dir = self.dir.join(name);
            let problem = platform::create_owner_only_dir(&dir).map_err(store_error(format!(
                "cannot create {what} {}",
                dir.display()
            )))?;
            refuse_problem(&dir, what, problem)?;
        } else if durable_record_dirs().contains(&key) {
            return Ok(());
        }
        sync(&self.dir)?;
        if let Some(parent) = self.dir.parent() {
            sync(parent)?;
        }
        let mut durable = durable_record_dirs();
        if !durable.contains(&key) {
            durable.push(key);
        }
        Ok(())
    }

    /// Checks that the store's directory `name`, which `what` names, is private to the current
    /// user, and not a link, if it exists, and returns whether it exists.
    fn check_records_dir(&self, name: &str, what: &str) -> Result<bool> {
        let dir = self.dir.join(name);
        match platform::check_owner_only_dir(&dir) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            checked => {
                let problem = checked
                    .map_err(store_error(format!("cannot open {what} {}", dir.display())))?;
                refuse_problem(&dir, what, problem)?;
                Ok(true)
            }
        }
    }

    /// Lists the store's directory `name`, which `what` names, whose records `parse` names: the
    /// records, and the temporary files that interrupted replacements of them left. A directory
    /// that does not exist holds none.
    ///
    /// Fails with [`ErrorCode::ForeignFiles`] for any other entry.
    fn records<T>(
        &self,
        name: &str,
        what: &str,
        parse: impl Fn(&str) -> Option<T>,
    ) -> Result<Records<T>> {
        let mut listed = Records {
            records: Vec::new(),
            temporaries: Vec::new(),
        };
        if !self.check_records_dir(name, what)? {
            return Ok(listed);
        }
        let dir = self.dir.join(name);
        let mut foreign = Vec::new();
        for entry in entries(&dir)? {
            if let Some(record) = parse(&entry) {
                listed.records.push(record);
            } else if is_temporary_of(&entry, |file| parse(file).is_some()) {
                listed.temporaries.push(dir.join(entry));
            } else {
                foreign.push(entry);
            }
        }
        if foreign.is_empty() {
            return Ok(listed);
        }
        foreign.sort();
        Err(Error::new(
            ErrorCode::ForeignFiles,
            format!(
                "{what} {} holds entries that are not the store's: {}",
                dir.display(),
                foreign.join(", ")
            ),
        ))
    }

    /// Reads the record of the lease `id`, or returns `None` if it does not exist.
    fn read_lease(&self, id: LeaseId) -> Result<Option<Lease>> {
        let path = self.lease_path(id);
        let Some(lease) = read_lease(&path, LEASE_DESCRIPTION)? else {
            return Ok(None);
        };
        if lease.id != id || lease.store != self.id {
            return Err(Error::io(format!(
                "the {LEASE_DESCRIPTION} {} is malformed: it is the lease {} of the template \
                 store {}",
                path.display(),
                lease.id,
                lease.store
            )));
        }
        Ok(Some(lease))
    }

    /// Returns whether a tombstone quarantines the generation `generation`.
    fn is_tombstoned(&self, generation: StoreGeneration) -> Result<bool> {
        if !self.check_records_dir(TOMBSTONES_NAME, TOMBSTONES_WHAT)? {
            return Ok(false);
        }
        self.read_tombstone(generation)
    }

    /// Reads the tombstone of the generation `generation`, in the directory of the tombstones,
    /// which exists, and returns whether it exists: a tombstone that cannot be read, is malformed,
    /// or is of another format fails.
    fn read_tombstone(&self, generation: StoreGeneration) -> Result<bool> {
        let path = self.tombstone_path(generation);
        let Some(record) =
            read_record::<TombstoneRecord>(&path, TOMBSTONE_DESCRIPTION, MAX_LEASE_RECORD_LEN)?
        else {
            return Ok(false);
        };
        if record.store != self.id.to_string() || record.generation != generation.get().get() {
            return Err(Error::io(format!(
                "the {TOMBSTONE_DESCRIPTION} {} is malformed: it is the tombstone of the \
                 generation {} of the template store {}",
                path.display(),
                record.generation,
                record.store
            )));
        }
        Ok(true)
    }

    /// Writes the tombstone of the generation `generation`, unless it has one, under the store's
    /// lock, and makes it durable with `sync`, even one that an earlier transaction wrote, whose
    /// sync may have failed.
    fn write_tombstone(&self, generation: StoreGeneration, sync: SyncDir<'_>) -> Result<()> {
        if !self.is_tombstoned(generation)? {
            self.create_records_dir(TOMBSTONES_NAME, TOMBSTONES_WHAT, sync)?;
            write_record(
                &self.tombstone_path(generation),
                &TombstoneRecord {
                    format: STORE_FORMAT,
                    store: self.id.to_string(),
                    generation: generation.get().get(),
                },
                "the tombstone of the template store",
            )?;
        }
        sync(&self.dir.join(TOMBSTONES_NAME))
    }

    /// Removes the tombstone of the generation `generation`, if it has one, under the store's
    /// lock, once it has read and checked it: a tombstone that is not the store's tombstone of
    /// that generation, or that is of another format, fails, and is kept.
    fn remove_tombstone(&self, generation: StoreGeneration) -> Result<()> {
        if self.is_tombstoned(generation)? {
            remove_if_present(&self.tombstone_path(generation))?;
        }
        Ok(())
    }

    /// Removes the payload of the generation `generation`, if it exists, under the store's lock.
    /// A payload that is a link, or not private to the current user, is refused, and kept.
    fn remove_payload(&self, generation: StoreGeneration) -> Result<()> {
        self.check_generations_dir()?;
        let payload = self.payload_dir(generation);
        let what = "the payload of the generation";
        match platform::check_owner_only_dir(&payload) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            checked => {
                let problem = checked.map_err(store_error(format!(
                    "cannot open {what} {}",
                    payload.display()
                )))?;
                refuse_problem(&payload, what, problem)?;
            }
        }
        match fs::remove_dir_all(&payload) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(store_error(format!(
                "cannot remove {what} {}",
                payload.display()
            ))(error)),
            _ => Ok(()),
        }
    }
}

/// Returns the error of a lease that the sandbox in `dir` cannot take or end while it records a
/// VM or a launch, [`ErrorCode::SandboxRunning`].
fn may_run(dir: &Path) -> Error {
    Error::new(
        ErrorCode::SandboxRunning,
        format!(
            "a VM of the sandbox in {}, or a launch of it, may still run",
            dir.display()
        ),
    )
}

/// Returns whether the clone binding of the sandbox of `lease` names it still: unless the sandbox
/// is gone, holds no clone binding, or holds one of another lease.
fn is_bound(lease: &Lease) -> Result<bool> {
    Ok(read_binding(&lease.sandbox)?
        .is_some_and(|binding| binding.id == lease.id && binding.store == lease.store))
}

/// Returns the directories whose entries changed as the clone binding of the sandbox in `sandbox`
/// went: the sandbox's own directory, if it is one, and the directory that holds the sandbox's
/// entry, the nearest of its ancestors that is a directory, following the links that its path
/// names, as the removal of the sandbox followed them. A sandbox that was removed, and provisioned
/// again at its path, is a new directory, whose entries never held the binding: the binding went
/// with the directory that the entry of the sandbox named before.
///
/// Fails for an ancestor that cannot be examined, or that is a link to nothing, whose target, the
/// directory that held the sandbox, cannot be known.
fn binding_dirs(sandbox: &Path) -> Result<Vec<&Path>> {
    let mut dirs = Vec::with_capacity(2);
    if fs::symlink_metadata(sandbox).is_ok_and(|metadata| metadata.is_dir()) {
        dirs.push(sandbox);
    }
    for ancestor in sandbox.ancestors().skip(1) {
        let unknown = |error: io::Error| {
            Error::io(format!(
                "cannot find the directory from which the sandbox in {} went, through {}",
                sandbox.display(),
                ancestor.display()
            ))
            .with_source(error)
        };
        match fs::metadata(ancestor) {
            Ok(metadata) if metadata.is_dir() => {
                dirs.push(ancestor);
                return Ok(dirs);
            }
            // A file replaced the directory, in the entries of its parent.
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if fs::symlink_metadata(ancestor).is_ok() {
                    return Err(unknown(error));
                }
            }
            Err(error) => return Err(unknown(error)),
        }
    }
    Err(Error::io(format!(
        "no directory holds the sandbox in {}",
        sandbox.display()
    )))
}

/// Writes `record` as the clone binding of the sandbox in `dir`, and makes it durable with `sync`.
fn bind(dir: &Path, record: &LeaseRecord, sync: SyncDir<'_>) -> Result<()> {
    write_record(&dir.join(CLONE_BINDING_NAME), record, "the clone binding")?;
    sync(dir)
}

/// Removes the clone binding of the sandbox in `dir`, if it has one, and makes its removal durable
/// with `sync`, before a record of its lease goes: a binding that outlasted the record through a
/// crash would let a retirement delete the generation that it names.
fn unbind(dir: &Path, sync: SyncDir<'_>) -> Result<()> {
    remove_if_present(&dir.join(CLONE_BINDING_NAME))?;
    sync(dir)
}

/// Removes the record of a lease at `path`, if it exists, which a release and a retirement, in
/// other processes, may both remove at once: a record that another removal removed as this tried,
/// as [`was_removed`] tells, is gone already.
fn remove_lease_record(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound && !was_removed(path, &error) => Err(
            store_error(format!("cannot remove {}", path.display()))(error),
        ),
        _ => Ok(()),
    }
}

/// Returns `error`, which ended an acquisition of the lease `id` whose binding the sandbox in
/// `dir` may hold still, with the error's code and source, saying so.
fn held_still(error: Error, dir: &Path, id: LeaseId) -> Error {
    let code = error.code();
    let (_, message, source) = error.into_parts();
    let error = Error::new(
        code,
        format!(
            "{message}; the sandbox in {} may hold the lease {id} still, which a release ends",
            dir.display()
        ),
    );
    match source {
        Some(source) => error.with_source(source),
        None => error,
    }
}

/// Reads the clone binding of the sandbox in `dir`, or returns `None` if it holds none, or if the
/// directory does not exist.
fn read_binding(dir: &Path) -> Result<Option<Lease>> {
    read_lease(&dir.join(CLONE_BINDING_NAME), BINDING_DESCRIPTION)
}

/// Reads the record of a lease at `path`, its `description`, or returns `None` if it does not
/// exist.
fn read_lease(path: &Path, description: &str) -> Result<Option<Lease>> {
    let Some(record) = read_record::<LeaseRecord>(path, description, MAX_LEASE_RECORD_LEN)? else {
        return Ok(None);
    };
    record.into_lease().map(Some).map_err(|problem| {
        Error::io(format!(
            "the {description} {} is malformed: {problem}",
            path.display()
        ))
    })
}

/// Parses the name of the record of a lease, `ID.json`, in the form in which the store writes
/// it.
fn lease_name(name: &str) -> Option<LeaseId> {
    let stem = name.strip_suffix(".json")?;
    let id: LeaseId = stem.parse().ok()?;
    (id.to_string() == stem).then_some(id)
}

/// Parses the name of a tombstone, `G.json`, in the form in which the store writes it.
fn tombstone_name(name: &str) -> Option<StoreGeneration> {
    let stem = name.strip_suffix(".json")?;
    let generation = NonZeroU64::new(stem.parse().ok()?).map(StoreGeneration::new)?;
    (generation.to_string() == stem).then_some(generation)
}

/// Returns whether `name` is a temporary file that an interrupted replacement of a record whose
/// name `record` accepts leaves beside it: `.NAME.NUMBER.tmp`.
fn is_temporary_of(name: &str, record: impl Fn(&str) -> bool) -> bool {
    name.strip_prefix('.')
        .and_then(|name| name.strip_suffix(".tmp"))
        .and_then(|name| name.rsplit_once('.'))
        .is_some_and(|(file, number)| {
            record(file) && !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
        })
}

/// The entries of the directory of a store's leases or tombstones.
struct Records<T> {
    /// What the names of the records name.
    records: Vec<T>,
    /// The temporary files that interrupted replacements of records left.
    temporaries: Vec<PathBuf>,
}

/// The record of a lease, as JSON: in its store, and as the clone binding of its sandbox.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LeaseRecord {
    /// [`STORE_FORMAT`].
    format: u32,
    /// The identity of the store that granted the lease.
    store: String,
    /// The identity of the lease.
    lease: String,
    /// The store generation of the leased generation.
    generation: u64,
    /// The identity of the leased generation's key.
    key: String,
    /// The directory of the sandbox that holds the lease, as an absolute path.
    sandbox: String,
}

impl LeaseRecord {
    /// Returns the record of `lease`.
    fn new(lease: &Lease) -> Self {
        Self {
            format: STORE_FORMAT,
            store: lease.store.to_string(),
            lease: lease.id.to_string(),
            generation: lease.generation.get().get(),
            key: lease.key.to_string(),
            sandbox: lease.sandbox.to_string_lossy().into_owned(),
        }
    }

    /// Checks the record, and returns its lease, or says why it is malformed.
    fn into_lease(self) -> std::result::Result<Lease, String> {
        let field =
            |what: &str, error: Error| format!("its {what} is invalid: {}", error.message());
        let sandbox = PathBuf::from(self.sandbox);
        if !sandbox.is_absolute() {
            return Err(format!(
                "its sandbox, {}, is not an absolute path",
                sandbox.display()
            ));
        }
        Ok(Lease {
            id: self
                .lease
                .parse()
                .map_err(|error| field("identity", error))?,
            store: self.store.parse().map_err(|error| field("store", error))?,
            generation: NonZeroU64::new(self.generation)
                .map(StoreGeneration::new)
                .ok_or("it leases store generation 0, which no generation has")?,
            key: self.key.parse().map_err(|error| field("key", error))?,
            sandbox,
        })
    }
}

/// A tombstone, as JSON.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TombstoneRecord {
    /// [`STORE_FORMAT`].
    format: u32,
    /// The identity of the store.
    store: String,
    /// The store generation of the tombstoned generation.
    generation: u64,
}
