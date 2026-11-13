//! The lease of a template's generation that the clone of a sandbox holds, as the sandbox's
//! [clone binding](CLONE_BINDING_NAME) records it, and the reading of the records of the template
//! store, in whose format the binding is.
//!
//! The sandbox's store reads the binding before it begins a launch. The template store,
//! [`template::Store`](crate::host::template::Store), grants and ends leases, and reads its own
//! records, with these, and [`template`](crate::host::template) re-exports what is public here.

use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::regular::{open_regular, open_regular_in, read_at_most, was_removed};
use super::{CLONE_BINDING_NAME, LaunchId, SandboxStore};
use crate::activation::{Identity, StoreGeneration, TemplateReference};
use crate::error::{Error, ErrorCode, ErrorKind, Result};
use crate::host::platform::{self, parse_hex_id, parse_hex32};

/// Version of the store's records: its identity record and its index.
pub const STORE_FORMAT: u32 = 1;

/// Name of the directory of the generations' payloads.
pub const GENERATIONS_NAME: &str = "generations";

/// Name of the store's identity record.
pub const STORE_RECORD_NAME: &str = "store.json";

/// Longest identity record, in bytes.
pub(crate) const MAX_STORE_RECORD_LEN: u64 = 4 * 1024;

/// Bytes of a [`StoreId`] and of a [`StorageGeneration`](crate::host::template::StorageGeneration).
pub(crate) const ID_LEN: usize = 16;

/// Longest record of a lease, of a clone binding, or of a tombstone, in bytes: more than a record
/// that names the longest path of a sandbox takes.
pub(crate) const MAX_LEASE_RECORD_LEN: u64 = 256 * 1024;

/// The binding of a clone in its sandbox, in messages.
pub(crate) const BINDING_DESCRIPTION: &str = "clone binding";

/// Returns `bytes` as lowercase hexadecimal digits.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The identity of a store: 16 random bytes that its creation draws.
///
/// Its text form, which `Display` writes and `FromStr` parses, is 32 lowercase hexadecimal
/// digits; parsing also accepts uppercase ones.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct StoreId([u8; ID_LEN]);

impl StoreId {
    /// Returns the identity whose bytes are `bytes`.
    pub const fn from_bytes(bytes: [u8; ID_LEN]) -> Self {
        Self(bytes)
    }

    /// Returns the bytes of the identity.
    pub const fn as_bytes(&self) -> &[u8; ID_LEN] {
        &self.0
    }
}

impl fmt::Display for StoreId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&hex(&self.0))
    }
}

impl fmt::Debug for StoreId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "StoreId({self})")
    }
}

impl FromStr for StoreId {
    type Err = Error;

    /// Parses the text form of a store's identity, failing with
    /// [`ErrorKind::Unsupported`] for anything else.
    fn from_str(text: &str) -> Result<Self> {
        parse_hex_id(text).map(Self).ok_or_else(|| {
            Error::unsupported(format!(
                "{text:?} is not a template store's identity, which is 32 hexadecimal digits"
            ))
        })
    }
}

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
/// of one sandbox, whose VM restores it, and reserves the clone's launch.
///
/// The store keeps the record of each lease that it granted, and the sandbox keeps the same
/// record as its clone binding, [`CLONE_BINDING_NAME`], beside its launch journal. A lease names
/// its sandbox by the path of its directory with its links resolved, so that no removal or change
/// of a link, or of a directory that a `..` component passes through, hides the sandbox from a
/// [retirement](crate::host::template::Store::retire). A sandbox that holds a lease must not move,
/// and goes only through its store's [removal](super::LockedStore::remove), which makes the
/// binding's absence durable before the sandbox goes. The lease allows one launch of the clone, as
/// the identity that it reserves, [`launch`](Self::launch), under the lock that acquired it, and no
/// other launch of the sandbox until a release ends it, as
/// [`LockedStore::begin_launch`](super::LockedStore::begin_launch) requires.
#[derive(Clone, PartialEq, Eq)]
pub struct Lease {
    /// The lease's identity.
    id: LeaseId,
    /// The identity of the store that granted it.
    store: StoreId,
    /// The store generation of the leased generation.
    generation: StoreGeneration,
    /// The identity of the leased generation's key.
    key: Identity,
    /// The directory of the sandbox that holds the lease, as an absolute path with its links
    /// resolved, which is valid Unicode.
    sandbox: PathBuf,
    /// The launch of the clone that the lease reserves.
    launch: LaunchId,
}

impl Lease {
    /// Returns the lease `id`, which the store `store` granted, of its generation `generation`,
    /// whose key's identity is `key`, to the sandbox in `sandbox`, an absolute path with its links
    /// resolved, which is valid Unicode, for the launch `launch` of its clone.
    pub(crate) fn new(
        id: LeaseId,
        store: StoreId,
        generation: StoreGeneration,
        key: Identity,
        sandbox: PathBuf,
        launch: LaunchId,
    ) -> Self {
        Self {
            id,
            store,
            generation,
            key,
            sandbox,
            launch,
        }
    }

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

    /// Returns the directory of the sandbox that holds the lease, as an absolute path with its
    /// links resolved, which may differ from the path through which the sandbox was leased, and
    /// through which a [release](crate::host::template::Store::release) takes the sandbox's lock.
    pub fn sandbox(&self) -> &Path {
        &self.sandbox
    }

    /// Returns the launch of the clone that the lease reserves: the identity under which the
    /// caller launches the clone, with [`supervisor::launch`](crate::host::supervisor::launch),
    /// once, under the lock that acquired the lease, and the only launch of the sandbox that
    /// [`LockedStore::begin_launch`](super::LockedStore::begin_launch) begins while the sandbox
    /// holds the lease.
    pub fn launch(&self) -> &LaunchId {
        &self.launch
    }

    /// Returns the reference by which the activation record of the clone names the template that
    /// it restores: the leased generation's store generation, and its key's identity.
    pub fn template(&self) -> TemplateReference {
        TemplateReference {
            generation: self.generation,
            key: self.key,
        }
    }
}

impl fmt::Debug for Lease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Lease")
            .field("id", &self.id)
            .field("store", &self.store)
            .field("generation", &self.generation)
            // The key writes itself as the `KeyIdentity` that `Lease::key` returns.
            .field("key", &format_args!("KeyIdentity({})", self.key))
            .field("sandbox", &self.sandbox)
            .field("launch", &self.launch)
            .finish()
    }
}

/// Returns the name of the control socket of the VMM of the clone whose launch is `launch`, in the
/// payload of the leased generation: `<launch>.sock`.
pub(crate) fn clone_socket_name(launch: &LaunchId) -> String {
    format!("{launch}.sock")
}

/// Returns whether `path` names the control socket of the VMM of the clone that holds `lease`, as
/// [`template::Store::clone_socket_path`](crate::host::template::Store::clone_socket_path) returns
/// it: `generations/<generation>/<launch>.sock`, in a template store, which a lease names by its
/// identity alone.
pub(crate) fn is_clone_socket(lease: &Lease, path: &Path) -> bool {
    let name = clone_socket_name(&lease.launch);
    let generation = lease.generation.to_string();
    let payload = path.parent();
    path.file_name() == Some(OsStr::new(&name))
        && payload.and_then(Path::file_name) == Some(OsStr::new(&generation))
        && payload.and_then(Path::parent).and_then(Path::file_name)
            == Some(OsStr::new(GENERATIONS_NAME))
}

/// Opens the payload of the generation `generation` of the template store `id` in `store`, which
/// holds the control sockets of the VMMs of the generation's clones on Linux:
/// `generations/<generation>`, for reading, so that the caller can sync it once it removed a
/// socket from it.
///
/// It follows no link at the store's own name, at the directory of the generations, or at the
/// payload, and fails if one of them is a link, or not a directory, so that a link that takes the
/// place of one of them never leads a removal to the files of another directory. It reads the
/// store's identity through the handle of the store's directory, failing if it cannot read the
/// identity record, and returns `None` if that directory holds another store, or none, as it does
/// if the store, its directory of the generations, or the payload is gone: the payload of a store
/// that is gone holds no socket. It opens the store's directory and the directory of the
/// generations with the right to search them alone, which an owner-only directory may grant
/// without the right to read it.
pub(crate) fn open_payload(
    store: &Path,
    id: StoreId,
    generation: StoreGeneration,
) -> Result<Option<platform::Directory>> {
    let payload = store.join(GENERATIONS_NAME).join(generation.to_string());
    let failed = || {
        store_error(format!(
            "cannot open the payload of the generation {}",
            payload.display()
        ))
    };
    let Some(dir) = platform::Directory::search(store).map_err(failed())? else {
        return Ok(None);
    };
    if read_identity_in(&dir, store)? != Some(id) {
        return Ok(None);
    }
    let Some(generations) = dir.search_dir(GENERATIONS_NAME).map_err(failed())? else {
        return Ok(None);
    };
    generations
        .open_dir(&generation.to_string())
        .map_err(failed())
}

/// Reads the identity of the store in `dir` from its identity record, or returns `None` if the
/// directory holds none.
pub(crate) fn read_identity(dir: &Path) -> Result<Option<StoreId>> {
    let path = dir.join(STORE_RECORD_NAME);
    read_identity_with(&path, || open_existing(&path))
}

/// Reads the identity of the store whose directory `directory` holds open, in `dir`, as
/// [`read_identity`] reads it at its path, or returns `None` if the directory holds no identity
/// record, or, on Windows, one whose deletion is pending: through the handle, so that it reads
/// the identity of the directory that the handle holds, whatever `dir` names meanwhile.
pub(crate) fn read_identity_in(
    directory: &platform::Directory,
    dir: &Path,
) -> Result<Option<StoreId>> {
    read_identity_with(&dir.join(STORE_RECORD_NAME), || {
        open_regular_in(directory, STORE_RECORD_NAME)
    })
}

/// Reads the identity of a store from its identity record at `path`, which `open` opens, or
/// returns `None` if `open` finds none.
fn read_identity_with(
    path: &Path,
    open: impl FnOnce() -> io::Result<Option<File>>,
) -> Result<Option<StoreId>> {
    let description = "identity record of the template store";
    let Some(record) =
        read_record_with::<StoreRecord>(path, description, MAX_STORE_RECORD_LEN, open)?
    else {
        return Ok(None);
    };
    record.store.parse().map(Some).map_err(|error: Error| {
        Error::io(format!(
            "the {description} {} is malformed: {}",
            path.display(),
            error.message()
        ))
    })
}

/// The identity record of a store, as JSON.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StoreRecord {
    /// [`STORE_FORMAT`].
    pub(crate) format: u32,
    /// The store's identity.
    pub(crate) store: String,
}

/// Reads the clone binding of the sandbox in `dir`, or returns `None` if it holds none, or if the
/// directory does not exist.
pub(crate) fn read_binding(dir: &Path) -> Result<Option<Lease>> {
    read_lease(&dir.join(CLONE_BINDING_NAME), BINDING_DESCRIPTION)
}

/// Reads the clone binding of the sandbox whose directory `directory` holds open, in `dir`, as
/// [`read_binding`] reads it at its path, or returns `None` if the directory holds none, or, on
/// Windows, one whose deletion is pending, which is removed: through the handle, so that it reads
/// the binding of the directory that the handle holds, whatever `dir` names meanwhile.
pub(crate) fn read_binding_in(
    directory: &platform::Directory,
    dir: &Path,
) -> Result<Option<Lease>> {
    read_lease_with(&dir.join(CLONE_BINDING_NAME), BINDING_DESCRIPTION, || {
        open_regular_in(directory, CLONE_BINDING_NAME)
    })
}

/// Reads the record of a lease at `path`, its `description`, or returns `None` if it does not
/// exist.
pub(crate) fn read_lease(path: &Path, description: &str) -> Result<Option<Lease>> {
    read_lease_with(path, description, || open_existing(path))
}

/// Reads the record of a lease, its `description` at `path`, from the file that `open` opens, or
/// returns `None` if `open` finds none.
fn read_lease_with(
    path: &Path,
    description: &str,
    open: impl FnOnce() -> io::Result<Option<File>>,
) -> Result<Option<Lease>> {
    let Some(record) =
        read_record_with::<LeaseRecord>(path, description, MAX_LEASE_RECORD_LEN, open)?
    else {
        return Ok(None);
    };
    record.into_lease().map(Some).map_err(|problem| {
        Error::io(format!(
            "the {description} {} is malformed: {problem}",
            path.display()
        ))
    })
}

/// The record of a lease, as JSON: in its store, and as the clone binding of its sandbox.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LeaseRecord {
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
    /// The launch of the clone that the lease reserves.
    launch: String,
}

impl LeaseRecord {
    /// Returns the record of `lease`.
    pub(crate) fn new(lease: &Lease) -> Self {
        Self {
            format: STORE_FORMAT,
            store: lease.store.to_string(),
            lease: lease.id.to_string(),
            generation: lease.generation.get().get(),
            key: lease.key.to_string(),
            sandbox: lease.sandbox.to_string_lossy().into_owned(),
            launch: lease.launch.to_string(),
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
            key: parse_key(&self.key).map_err(|error| field("key", error))?,
            sandbox,
            launch: LaunchId::try_from(self.launch)
                .map_err(|problem| format!("its launch is invalid: {problem}"))?,
        })
    }
}

/// Parses the text form of the identity of a template's key, 64 hexadecimal digits, as
/// [`KeyIdentity`](crate::host::template::KeyIdentity) parses it, failing with
/// [`ErrorKind::Unsupported`] for anything else.
fn parse_key(text: &str) -> Result<Identity> {
    parse_hex32(text).map(Identity::from_bytes).ok_or_else(|| {
        Error::unsupported(format!(
            "{text:?} is not a template key's identity, which is 64 hexadecimal digits"
        ))
    })
}

/// Builds an error mapper that keeps an I/O source under `context`, as an error of
/// [`ErrorKind::NotFound`] if the source says that a path does not exist, and of
/// [`ErrorKind::Io`] otherwise.
pub(crate) fn store_error(context: String) -> impl FnOnce(io::Error) -> Error {
    move |error| {
        let kind = match error.kind() {
            io::ErrorKind::NotFound => ErrorKind::NotFound,
            _ => ErrorKind::Io,
        };
        Error::new(kind, context).with_source(error)
    }
}

/// Reads the private regular file at `path`, the store's `description`, of at most `limit` bytes,
/// or returns `None` if it does not exist, or was removed as this opened it, as [`was_removed`]
/// tells, which a transaction in another process may do.
///
/// The file is opened without following a link, or on Windows any other reparse point, at its
/// own name, and refused unless it is a regular file, before anything is read from it. A file
/// that is not private to the current user is refused, since others could have written it: on
/// Windows, a file's own access control list decides who may open it, whatever the store's
/// directory allows.
pub(crate) fn read_private(path: &Path, description: &str, limit: u64) -> Result<Option<Vec<u8>>> {
    read_private_with(path, description, limit, || open_existing(path))
}

/// Opens the regular file at `path`, as [`open_regular`] does, or returns `None` if it does not
/// exist, or was removed as this opened it, as [`was_removed`] tells.
fn open_existing(path: &Path) -> io::Result<Option<File>> {
    match open_regular(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::NotFound || was_removed(path, &error) => {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Reads the private regular file that `open` opens, the store's `description` at `path`, of at
/// most `limit` bytes, as [`read_private`] reads it, or returns `None` if `open` finds none.
fn read_private_with(
    path: &Path,
    description: &str,
    limit: u64,
    open: impl FnOnce() -> io::Result<Option<File>>,
) -> Result<Option<Vec<u8>>> {
    let failed = || store_error(format!("cannot read the {description} {}", path.display()));
    let Some(file) = open().map_err(failed())? else {
        return Ok(None);
    };
    if let Some(problem) = platform::private_file_problem(&file).map_err(failed())? {
        return Err(Error::io(format!(
            "the {description} {} {problem}",
            path.display()
        )));
    }
    read_at_most(file, limit, &format!("the {description}"))
        .map(Some)
        .map_err(failed())
}

/// Reads the store's record at `path`, its `description`, of at most `limit` bytes, in format
/// [`STORE_FORMAT`], or returns `None` if it does not exist.
pub(crate) fn read_record<T: DeserializeOwned>(
    path: &Path,
    description: &str,
    limit: u64,
) -> Result<Option<T>> {
    read_record_with(path, description, limit, || open_existing(path))
}

/// Reads the store's record that `open` opens, its `description` at `path`, as [`read_record`]
/// reads it, or returns `None` if `open` finds none.
fn read_record_with<T: DeserializeOwned>(
    path: &Path,
    description: &str,
    limit: u64,
    open: impl FnOnce() -> io::Result<Option<File>>,
) -> Result<Option<T>> {
    let Some(text) = read_private_with(path, description, limit, open)? else {
        return Ok(None);
    };
    let malformed = || format!("the {description} {} is malformed", path.display());
    let value: serde_json::Value =
        serde_json::from_slice(&text).map_err(|error| Error::io(malformed()).with_source(error))?;
    match value.get("format") {
        Some(format) if format.as_u64() == Some(STORE_FORMAT.into()) => {}
        Some(format) => {
            return Err(Error::new(
                ErrorCode::FormatUnsupported,
                format!(
                    "the {description} {} has the unsupported format {format}; this build reads \
                     format {STORE_FORMAT} only and migrates no other, so create a new template \
                     store, and capture its templates again",
                    path.display()
                ),
            ));
        }
        None => return Err(Error::io(format!("{}: it has no format", malformed()))),
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(|error| Error::io(malformed()).with_source(error))
}
