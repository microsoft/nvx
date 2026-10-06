//! Registry of guest images, which keeps hashing off the sandbox start path.
//!
//! ```text
//! <state_root>/nvxhost/images/
//!   .lock                     serializes adding, removing, and using registrations
//!   <sha256>.json             path and seal of one registered image content
//! ```
//!
//! An image is hashed once, when it is registered, or not at all when the caller supplies a
//! digest that its own policy already verified. The registration keeps the file's [`FileSeal`];
//! provisioning and starting a sandbox compare the file's current seal with it and fail closed
//! when they differ, rather than hashing the image again. The backend checks its OpenVMM runtime
//! files the same way: it hashes them once, when it is created, and later compares seals.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufReader, Seek};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

use super::platform::{self, FileSeal};
use super::state::{self, LockGuard};
use crate::error::{Error, Result};

const RECORD_FORMAT: u32 = 1;
const LOCK_NAME: &str = ".lock";
const RECORD_SUFFIX: &str = ".json";
const HASH_BUFFER_BYTES: usize = 1 << 20;

/// Content identity of a registered guest image: the SHA-256 digest of the image file.
///
/// An ID displays and parses as `sha256:` followed by 64 lowercase hexadecimal digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ImageId([u8; 32]);

impl ImageId {
    const PREFIX: &'static str = "sha256:";

    /// Returns the ID of image content with this SHA-256 digest.
    pub const fn from_sha256(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Returns the SHA-256 digest of the image content.
    pub const fn sha256(&self) -> &[u8; 32] {
        &self.0
    }

    /// Parses an ID, returning
    /// [`ErrorCode::MalformedRequest`](crate::ErrorCode::MalformedRequest) unless it is `sha256:`
    /// followed by 64 lowercase hexadecimal digits.
    pub fn parse(value: &str) -> Result<Self> {
        value
            .strip_prefix(Self::PREFIX)
            .and_then(decode_digest)
            .map(Self)
            .ok_or_else(|| {
                Error::malformed_request(format!(
                    "image ID {value:?} must be sha256: followed by 64 lowercase hexadecimal \
                     digits"
                ))
            })
    }
}

impl fmt::Display for ImageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}{}", Self::PREFIX, encode_hex(&self.0))
    }
}

impl fmt::Debug for ImageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ImageId({self})")
    }
}

impl FromStr for ImageId {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

impl Serialize for ImageId {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ImageId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(|error| serde::de::Error::custom(error.message()))
    }
}

/// How the backend establishes the content digest of an image that it registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ImageDigest {
    /// Hashes the file.
    #[default]
    Compute,
    /// Hashes the file and requires this SHA-256 digest.
    Expect([u8; 32]),
    /// Records this SHA-256 digest without reading the file.
    ///
    /// Use it only for content that the caller's own policy has verified and protects from
    /// writers, for example a file that an installer hashed before making it read-only.
    Trusted([u8; 32]),
}

/// A registered guest image.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RegisteredImage {
    /// Content identity.
    pub id: ImageId,
    /// Absolute path that sandboxes attach the image from.
    pub path: PathBuf,
    /// Length in bytes at registration.
    pub length: u64,
    /// Whether the caller supplied the digest instead of the backend hashing the file.
    pub trusted: bool,
    /// Whether the file still matches its registration. A sandbox whose image changed does not
    /// start until the image is registered again.
    pub intact: bool,
}

/// Persisted registration of one image content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ImageRecord {
    format: u32,
    pub(crate) id: ImageId,
    pub(crate) path: PathBuf,
    trusted: bool,
    seal: FileSeal,
}

impl ImageRecord {
    pub(crate) fn describe(&self, intact: bool) -> RegisteredImage {
        RegisteredImage {
            id: self.id,
            path: self.path.clone(),
            length: self.seal.length,
            trusted: self.trusted,
            intact,
        }
    }

    fn subject(&self) -> String {
        format!("image {} at {}", self.id, self.path.display())
    }

    /// Hashes the image through `file`, a handle from [`ImageStore::open_checked`], and compares
    /// the digest with the image's ID.
    pub(crate) fn verify_content(&self, file: &mut File) -> Result<()> {
        verify_digest(file, &self.id.0, &self.subject())
    }
}

/// Directory of image registrations shared by every backend with the same state root.
#[derive(Debug)]
pub(crate) struct ImageStore {
    dir: PathBuf,
}

impl ImageStore {
    /// Opens the registry in `dir`, creating it and restricting it to the current user if needed.
    pub(crate) fn open(dir: &Path) -> io::Result<Self> {
        platform::create_private_dir(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }

    /// Takes the registry lock, which serializes adding and removing registrations with recording
    /// sandboxes that use them.
    pub(crate) fn lock(&self) -> Result<LockGuard> {
        state::lock(&self.dir.join(LOCK_NAME))
    }

    fn record_path(&self, id: &ImageId) -> PathBuf {
        self.dir
            .join(format!("{}{RECORD_SUFFIX}", encode_hex(&id.0)))
    }

    /// Loads the registration of `id`, if there is one.
    pub(crate) fn get(&self, id: &ImageId) -> Result<Option<ImageRecord>> {
        let path = self.record_path(id);
        match state::read_json::<ImageRecord>(&path)? {
            Some(record) if record.format != RECORD_FORMAT || record.id != *id => {
                Err(Error::backend_error(format!(
                    "image record {} does not describe {id}",
                    path.display()
                )))
            }
            record => Ok(record),
        }
    }

    /// Loads every registration, ordered by ID.
    pub(crate) fn list(&self) -> Result<Vec<ImageRecord>> {
        let list_error = || {
            Error::backend_error(format!(
                "cannot list the image registry {}",
                self.dir.display()
            ))
        };
        let mut records = Vec::new();
        for entry in fs::read_dir(&self.dir).map_err(|error| list_error().with_source(error))? {
            let name = entry
                .map_err(|error| list_error().with_source(error))?
                .file_name();
            let Some(digest) = name
                .to_str()
                .and_then(|name| name.strip_suffix(RECORD_SUFFIX))
                .and_then(decode_digest)
            else {
                continue;
            };
            // A registration removed since the listing no longer exists.
            if let Some(record) = self.get(&ImageId(digest))? {
                records.push(record);
            }
        }
        records.sort_by_key(|record| record.id);
        Ok(records)
    }

    /// Registers the image at the absolute `path`, reusing a registration that the file still
    /// matches without reading the file.
    ///
    /// The caller must not hold the registry lock, which this takes to record a new
    /// registration.
    pub(crate) fn register(&self, path: &Path, digest: ImageDigest) -> Result<ImageRecord> {
        let subject = format!("image {}", path.display());
        let mut file = open(path, true, &subject)?;
        let seal = seal(&file, &subject)?;
        if let Some(record) = self
            .list()?
            .into_iter()
            .find(|record| record.path == path && record.seal == seal)
        {
            return match digest {
                ImageDigest::Expect(expected) | ImageDigest::Trusted(expected)
                    if record.id.0 != expected =>
                {
                    Err(Error::backend_unavailable(format!(
                        "{subject} is registered as {}, not {}",
                        record.id,
                        ImageId(expected)
                    )))
                }
                _ => Ok(record),
            };
        }
        let (id, trusted) = match digest {
            ImageDigest::Trusted(expected) => (ImageId(expected), true),
            ImageDigest::Compute | ImageDigest::Expect(_) => {
                let id = ImageId(hash(&mut file, &subject)?);
                if self::seal(&file, &subject)? != seal {
                    return Err(Error::backend_unavailable(format!(
                        "{subject} changed while it was hashed"
                    )));
                }
                if let ImageDigest::Expect(expected) = digest
                    && id.0 != expected
                {
                    return Err(Error::backend_unavailable(format!(
                        "{subject} is {id}, not the expected {}",
                        ImageId(expected)
                    )));
                }
                (id, false)
            }
        };
        let record = ImageRecord {
            format: RECORD_FORMAT,
            id,
            path: path.to_path_buf(),
            trusted,
            seal,
        };
        let _registry = self.lock()?;
        state::write_json(&self.record_path(&id), &record)?;
        Ok(record)
    }

    /// Opens a registered image and checks that it still matches its registration.
    ///
    /// With `deny_writers`, the handle keeps writers out on Windows until it is dropped.
    pub(crate) fn open_checked(&self, record: &ImageRecord, deny_writers: bool) -> Result<File> {
        open_sealed(
            &record.path,
            &record.seal,
            deny_writers,
            &record.subject(),
            "it was registered; register it again",
        )
    }

    /// Hashes a registered image and compares the digest with its ID.
    pub(crate) fn verify(&self, record: &ImageRecord) -> Result<()> {
        record.verify_content(&mut self.open_checked(record, true)?)
    }

    /// Removes the registration of `id`, returning whether it existed.
    ///
    /// The caller holds the registry lock and has checked that no sandbox uses the image.
    pub(crate) fn remove(&self, id: &ImageId) -> Result<bool> {
        let path = self.record_path(id);
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(
                Error::backend_error(format!("cannot remove {}", path.display()))
                    .with_source(error),
            ),
        }
    }
}

/// A runtime file that the backend hashed once, with the seal that it had then.
#[derive(Debug)]
pub(crate) struct VerifiedFile {
    subject: String,
    path: PathBuf,
    sha256: [u8; 32],
    seal: FileSeal,
}

impl VerifiedFile {
    /// Hashes the file at `path`, requiring the `approved` digest if there is one.
    pub(crate) fn verify(
        path: &Path,
        description: &str,
        approved: Option<[u8; 32]>,
    ) -> Result<Self> {
        let subject = format!("the {description} {}", path.display());
        let mut file = open(path, true, &subject)?;
        let seal = seal(&file, &subject)?;
        let sha256 = hash(&mut file, &subject)?;
        if self::seal(&file, &subject)? != seal {
            return Err(Error::backend_unavailable(format!(
                "{subject} changed while it was hashed"
            )));
        }
        if approved.is_some_and(|approved| approved != sha256) {
            return Err(Error::backend_unavailable(format!(
                "{subject} does not match the approved SHA-256"
            )));
        }
        Ok(Self {
            subject,
            path: path.to_path_buf(),
            sha256,
            seal,
        })
    }

    pub(crate) fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }

    /// Opens the file, keeping writers out on Windows until the handle is dropped, and checks
    /// that it has not changed since it was hashed.
    pub(crate) fn open_checked(&self) -> Result<File> {
        open_sealed(
            &self.path,
            &self.seal,
            true,
            &self.subject,
            "the backend verified it; create the backend again",
        )
    }

    /// Hashes the file again through `file`, a handle from [`Self::open_checked`].
    pub(crate) fn verify_content(&self, file: &mut File) -> Result<()> {
        verify_digest(file, &self.sha256, &self.subject)
    }
}

fn open(path: &Path, deny_writers: bool, subject: &str) -> Result<File> {
    platform::open_sealable(path, deny_writers).map_err(|error| {
        Error::backend_unavailable(format!("cannot open {subject}: {error}")).with_source(error)
    })
}

fn seal(file: &File, subject: &str) -> Result<FileSeal> {
    platform::file_seal(file).map_err(|error| {
        Error::backend_unavailable(format!("cannot inspect {subject}: {error}")).with_source(error)
    })
}

/// Opens `path` and checks that its seal is still `expected`.
fn open_sealed(
    path: &Path,
    expected: &FileSeal,
    deny_writers: bool,
    subject: &str,
    since: &str,
) -> Result<File> {
    let file = open(path, deny_writers, subject)?;
    if seal(&file, subject)? == *expected {
        Ok(file)
    } else {
        Err(Error::backend_unavailable(format!(
            "{subject} changed since {since}"
        )))
    }
}

fn verify_digest(file: &mut File, expected: &[u8; 32], subject: &str) -> Result<()> {
    if hash(file, subject)? == *expected {
        Ok(())
    } else {
        Err(Error::backend_unavailable(format!(
            "{subject} no longer matches its SHA-256"
        )))
    }
}

/// Hashes a whole file from its start.
fn hash(file: &mut File, subject: &str) -> Result<[u8; 32]> {
    fn digest(file: &mut File) -> io::Result<[u8; 32]> {
        file.rewind()?;
        let mut hasher = Sha256::new();
        io::copy(
            &mut BufReader::with_capacity(HASH_BUFFER_BYTES, file),
            &mut hasher,
        )?;
        Ok(hasher.finalize().into())
    }
    digest(file).map_err(|error| {
        Error::backend_unavailable(format!("cannot hash {subject}: {error}")).with_source(error)
    })
}

/// Returns `bytes` as lowercase hexadecimal digits.
pub(crate) fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_digest(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let mut digest = [0u8; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(digest)
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::ErrorCode;

    fn sha256(bytes: &[u8]) -> [u8; 32] {
        Sha256::digest(bytes).into()
    }

    fn registry(root: &Path) -> ImageStore {
        ImageStore::open(&root.join("images")).unwrap()
    }

    fn image(root: &Path, name: &str, content: &[u8]) -> PathBuf {
        let path = root.join(name);
        fs::write(&path, content).unwrap();
        path
    }

    /// Moves the file's modification time without changing its content.
    fn touch(path: &Path) {
        let file = File::options().write(true).open(path).unwrap();
        let modified = file.metadata().unwrap().modified().unwrap();
        file.set_modified(modified + Duration::from_secs(2))
            .unwrap();
    }

    fn unavailable<T: fmt::Debug>(result: Result<T>, expected: &str) {
        let error = result.unwrap_err();
        assert_eq!(error.code(), ErrorCode::BackendUnavailable, "{error}");
        assert!(error.message().contains(expected), "{error}");
    }

    #[test]
    fn image_ids_display_parse_and_serialize_as_prefixed_digests() {
        let id = ImageId::from_sha256([0xab; 32]);
        let text = format!("sha256:{}", "ab".repeat(32));
        assert_eq!(id.to_string(), text);
        assert_eq!(ImageId::parse(&text).unwrap(), id);
        assert_eq!(text.parse::<ImageId>().unwrap(), id);
        assert_eq!(id.sha256(), &[0xab; 32]);
        assert_eq!(serde_json::to_string(&id).unwrap(), format!("\"{text}\""));
        assert_eq!(
            serde_json::from_str::<ImageId>(&format!("\"{text}\"")).unwrap(),
            id
        );
        for malformed in [
            String::new(),
            "ab".repeat(32),
            format!("sha512:{}", "ab".repeat(32)),
            format!("sha256:{}", "AB".repeat(32)),
            format!("sha256:{}", "ab".repeat(31)),
            format!("sha256:{}0", "ab".repeat(32)),
            format!("sha256:{}+0", "ab".repeat(31)),
        ] {
            let error = ImageId::parse(&malformed).unwrap_err();
            assert_eq!(error.code(), ErrorCode::MalformedRequest, "{malformed:?}");
            assert!(serde_json::from_str::<ImageId>(&format!("{malformed:?}")).is_err());
        }
    }

    #[test]
    fn registration_hashes_once_and_is_reused_while_the_file_is_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let path = image(root.path(), "image.vhd", b"image content");
        let store = registry(root.path());
        let record = store.register(&path, ImageDigest::Compute).unwrap();
        assert_eq!(record.id, ImageId(sha256(b"image content")));
        assert_eq!(
            record.describe(true),
            RegisteredImage {
                id: record.id,
                path: path.clone(),
                length: 13,
                trusted: false,
                intact: true,
            }
        );

        // Another process sees the same registration, and the expected digest matches it.
        let reopened = registry(root.path());
        assert_eq!(reopened.list().unwrap(), std::slice::from_ref(&record));
        assert_eq!(reopened.get(&record.id).unwrap(), Some(record.clone()));
        assert_eq!(
            reopened
                .register(&path, ImageDigest::Expect(record.id.0))
                .unwrap(),
            record
        );
        drop(reopened.open_checked(&record, true).unwrap());
        reopened.verify(&record).unwrap();
    }

    #[test]
    fn trusted_digests_are_recorded_without_hashing_and_then_reused() {
        let root = tempfile::tempdir().unwrap();
        let path = image(root.path(), "image.vhd", b"image content");
        let store = registry(root.path());
        // The content does not have this digest, so a registration that hashed would differ.
        let claimed = [7; 32];
        let record = store
            .register(&path, ImageDigest::Trusted(claimed))
            .unwrap();
        assert_eq!(record.id, ImageId(claimed));
        assert!(record.describe(true).trusted);
        assert_eq!(store.register(&path, ImageDigest::Compute).unwrap(), record);
        assert_eq!(
            store
                .register(&path, ImageDigest::Trusted(claimed))
                .unwrap(),
            record
        );
        // The diagnostic hash exposes a trusted digest that the content does not have.
        unavailable(store.verify(&record), "no longer matches its SHA-256");

        for conflicting in [
            ImageDigest::Trusted(sha256(b"image content")),
            ImageDigest::Expect(sha256(b"image content")),
        ] {
            unavailable(
                store.register(&path, conflicting),
                &format!("is registered as {}", record.id),
            );
        }
        assert_eq!(store.list().unwrap(), [record]);
    }

    #[test]
    fn an_unexpected_digest_registers_nothing() {
        let root = tempfile::tempdir().unwrap();
        let path = image(root.path(), "image.vhd", b"image content");
        let store = registry(root.path());
        unavailable(
            store.register(&path, ImageDigest::Expect([1; 32])),
            &format!("not the expected {}", ImageId([1; 32])),
        );
        assert!(store.list().unwrap().is_empty());
        unavailable(
            store.register(&root.path().join("missing.vhd"), ImageDigest::Compute),
            "cannot open image",
        );
        unavailable(store.register(root.path(), ImageDigest::Compute), "image");
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn a_changed_image_fails_closed_until_it_is_registered_again() {
        let root = tempfile::tempdir().unwrap();
        let path = image(root.path(), "image.vhd", b"image content");
        let store = registry(root.path());
        let record = store.register(&path, ImageDigest::Compute).unwrap();

        // Only the timestamp moves: the content, and thus the ID, stays the same.
        touch(&path);
        unavailable(
            store.open_checked(&record, false),
            "changed since it was registered; register it again",
        );
        unavailable(store.verify(&record), "changed since it was registered");
        let refreshed = store.register(&path, ImageDigest::Compute).unwrap();
        assert_eq!(refreshed.id, record.id);
        assert_ne!(refreshed.seal, record.seal);
        assert_eq!(store.list().unwrap(), std::slice::from_ref(&refreshed));
        drop(store.open_checked(&refreshed, true).unwrap());

        // New content gets a new ID; the old registration stays, but no longer matches.
        File::options()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b" changed")
            .unwrap();
        unavailable(store.open_checked(&refreshed, true), "changed since");
        let changed = store.register(&path, ImageDigest::Compute).unwrap();
        assert_eq!(changed.id, ImageId(sha256(b"image content changed")));
        assert_eq!(store.list().unwrap().len(), 2);
        drop(store.open_checked(&changed, false).unwrap());
    }

    #[test]
    fn a_replaced_file_fails_closed_even_with_identical_content_and_times() {
        let root = tempfile::tempdir().unwrap();
        let path = image(root.path(), "image.vhd", b"image content");
        let store = registry(root.path());
        let record = store.register(&path, ImageDigest::Compute).unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let replacement = image(root.path(), "replacement.vhd", b"image content");
        File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        fs::remove_file(&path).unwrap();
        fs::rename(&replacement, &path).unwrap();
        unavailable(store.open_checked(&record, false), "changed since");
    }

    #[test]
    fn the_same_content_moves_to_its_latest_registered_location() {
        let root = tempfile::tempdir().unwrap();
        let first = image(root.path(), "first.vhd", b"image content");
        let second = image(root.path(), "second.vhd", b"image content");
        let store = registry(root.path());
        let original = store.register(&first, ImageDigest::Compute).unwrap();
        let moved = store.register(&second, ImageDigest::Compute).unwrap();
        assert_eq!(moved.id, original.id);
        assert_eq!(store.get(&original.id).unwrap().unwrap().path, second);
        assert_eq!(store.list().unwrap(), [moved]);
    }

    #[test]
    fn records_are_private_and_unrelated_files_are_ignored() {
        let root = tempfile::tempdir().unwrap();
        let path = image(root.path(), "image.vhd", b"image content");
        let store = registry(root.path());
        let record = store.register(&path, ImageDigest::Compute).unwrap();
        let dir = root.path().join("images");
        for name in [".lock", ".record.json.123.tmp", "notes.json", "README"] {
            fs::write(dir.join(name), b"not a record").unwrap();
        }
        assert_eq!(store.list().unwrap(), std::slice::from_ref(&record));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: PathBuf| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(dir.clone()), 0o700);
            assert_eq!(mode(store.record_path(&record.id)), 0o600);
        }

        fs::write(store.record_path(&record.id), b"{ truncated").unwrap();
        assert_eq!(store.list().unwrap_err().code(), ErrorCode::BackendError);
        let other = ImageId([9; 32]);
        fs::write(
            store.record_path(&other),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        fs::remove_file(store.record_path(&record.id)).unwrap();
        let error = store.get(&other).unwrap_err();
        assert!(error.message().contains("does not describe"), "{error}");
    }

    #[test]
    fn removal_reports_whether_a_registration_existed() {
        let root = tempfile::tempdir().unwrap();
        let path = image(root.path(), "image.vhd", b"image content");
        let store = registry(root.path());
        let record = store.register(&path, ImageDigest::Compute).unwrap();
        let _registry = store.lock().unwrap();
        assert!(store.remove(&record.id).unwrap());
        assert!(!store.remove(&record.id).unwrap());
        assert_eq!(store.get(&record.id).unwrap(), None);
        assert!(path.exists());
    }

    #[test]
    fn runtime_files_are_hashed_once_and_then_checked_by_seal() {
        let root = tempfile::tempdir().unwrap();
        let path = image(root.path(), "vmlinux", b"kernel");
        let verified = VerifiedFile::verify(&path, "guest kernel", None).unwrap();
        assert_eq!(verified.sha256(), &sha256(b"kernel"));
        assert_eq!(
            VerifiedFile::verify(&path, "guest kernel", Some(sha256(b"kernel")))
                .unwrap()
                .sha256(),
            &sha256(b"kernel")
        );
        unavailable(
            VerifiedFile::verify(&path, "guest kernel", Some([0; 32])),
            "the guest kernel",
        );
        let mut handle = verified.open_checked().unwrap();
        verified.verify_content(&mut handle).unwrap();
        drop(handle);

        touch(&path);
        unavailable(
            verified.open_checked(),
            "changed since the backend verified it; create the backend again",
        );
    }

    #[cfg(windows)]
    #[test]
    fn sealed_handles_keep_writers_out_and_writers_block_registration() {
        const ERROR_SHARING_VIOLATION: i32 = 32;
        let root = tempfile::tempdir().unwrap();
        let path = image(root.path(), "image.vhd", b"image content");
        let store = registry(root.path());
        let record = store.register(&path, ImageDigest::Compute).unwrap();

        let held = store.open_checked(&record, true).unwrap();
        let error = File::options().write(true).open(&path).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(ERROR_SHARING_VIOLATION));
        assert!(fs::remove_file(&path).is_err());
        // Readers, including a second start of the same image, still share it.
        drop(File::open(&path).unwrap());
        drop(store.open_checked(&record, true).unwrap());
        drop(held);

        let writer = File::options().write(true).open(&path).unwrap();
        unavailable(
            store.register(&path, ImageDigest::Compute),
            "cannot open image",
        );
        unavailable(store.open_checked(&record, true), "cannot open image");
        // Listing does not deny writers, so it still inspects an image that is open for writing.
        drop(store.open_checked(&record, false).unwrap());
        drop(writer);
    }

    /// Restoring the last-write time after a write preserves the seal on Windows, so only content
    /// verification detects the change.
    #[cfg(windows)]
    #[test]
    fn content_verification_detects_a_change_that_preserves_the_seal() {
        use std::os::windows::io::AsRawHandle;

        use windows_sys::Win32::Storage::FileSystem::{
            FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandleEx,
            SetFileInformationByHandle,
        };

        let root = tempfile::tempdir().unwrap();
        let path = image(root.path(), "image.vhd", b"image content");
        let store = registry(root.path());
        let record = store.register(&path, ImageDigest::Compute).unwrap();

        let mut file = File::options().read(true).write(true).open(&path).unwrap();
        let handle = file.as_raw_handle();
        let mut times = FILE_BASIC_INFO::default();
        let size = std::mem::size_of::<FILE_BASIC_INFO>() as u32;
        // SAFETY: the handle is open and the buffer is a FILE_BASIC_INFO of the size passed.
        assert_ne!(
            unsafe {
                GetFileInformationByHandleEx(handle, FileBasicInfo, (&raw mut times).cast(), size)
            },
            0
        );
        file.write_all(b"IMAGE").unwrap();
        // SAFETY: as above; the handle has write access.
        assert_ne!(
            unsafe {
                SetFileInformationByHandle(handle, FileBasicInfo, (&raw const times).cast(), size)
            },
            0
        );
        drop(file);
        assert_eq!(fs::read(&path).unwrap(), b"IMAGE content");

        drop(store.open_checked(&record, true).unwrap());
        unavailable(store.verify(&record), "no longer matches its SHA-256");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn special_files_are_refused_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let root = tempfile::tempdir().unwrap();
        let fifo = root.path().join("image.fifo");
        let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: the path is a NUL-terminated string.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let store = registry(root.path());
        unavailable(
            store.register(&fifo, ImageDigest::Compute),
            "does not name a regular file",
        );
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn seal_changes_are_visible_to_new_handles() {
        let root = tempfile::tempdir().unwrap();
        let path = image(root.path(), "image.vhd", b"image content");
        let sealed =
            |path: &Path| platform::file_seal(&platform::open_sealable(path, false).unwrap());
        let first = sealed(&path).unwrap();
        assert_eq!(sealed(&path).unwrap(), first);
        assert_eq!(first.length, 13);
        let modified = SystemTime::now() + Duration::from_secs(60);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        let second = sealed(&path).unwrap();
        assert_ne!(second.modified, first.modified);
        assert_eq!((&second.file, second.volume), (&first.file, first.volume));
    }

    /// Windows security components update a file's change time when they cache its hash in an
    /// extended attribute, so Windows seals ignore metadata-only changes. Linux seals keep the
    /// change time, which writers cannot restore.
    #[test]
    fn metadata_only_changes_alter_the_seal_only_where_the_change_time_is_sealed() {
        let root = tempfile::tempdir().unwrap();
        let path = image(root.path(), "image.vhd", b"image content");
        let sealed = |path: &Path| {
            platform::file_seal(&platform::open_sealable(path, false).unwrap()).unwrap()
        };
        let before = sealed(&path);
        // Let a coarse change-time clock advance.
        std::thread::sleep(Duration::from_millis(50));
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&path, permissions.clone()).unwrap();
        let after = sealed(&path);
        assert_eq!(after.modified, before.modified);
        assert_eq!(after == before, cfg!(windows), "{before:?} -> {after:?}");
        // A read-only file would outlive the temporary directory on Windows.
        #[cfg(windows)]
        {
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            fs::set_permissions(&path, permissions).unwrap();
        }
    }
}
