//! Tests of the template store: its creation and opening, its records and their formats, the
//! namespaces of its generations, its lock, and its refusal of directories and files that are
//! not private to the current user, or that are links.

use std::sync::{Arc, Barrier, mpsc};

use serde_json::{Value, json};

use super::*;
use crate::host::template::MANIFEST_NAME;
use crate::host::template::key::CaptureClass;
use crate::host::template::manifest::TemplateManifest;
use crate::host::template::test_support::{ManifestSpec, class};

/// A change to a record, as JSON.
type Change = fn(&mut Value);

/// Creates a store in a fresh temporary directory, and returns the directory and the store.
pub(super) fn store() -> (tempfile::TempDir, Store) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::create(directory.path().join("templates")).unwrap();
    (directory, store)
}

/// Returns the storage generation whose bytes are all `byte`, which must not be 0.
pub(super) fn storage(byte: u8) -> StorageGeneration {
    StorageGeneration::from_bytes([byte; ID_LEN]).unwrap()
}

/// Creates the directory `path` private to the current user, as the store creates its own.
pub(super) fn private_dir(path: &Path) {
    assert_eq!(platform::create_owner_only_dir(path).unwrap(), None);
}

/// Publishes a capture of `class` with the storage generation `storage`, whose manifest `spec`
/// encodes, as a publication does, under the store's lock: its snapshot at the place of the next
/// store generation, its key beside it, and then the index, which lists it.
pub(super) fn publish_with(
    store: &Store,
    class: &CaptureClass,
    spec: &ManifestSpec,
    storage: StorageGeneration,
) -> Generation {
    let _lock = take_lock(store.dir(), None).unwrap();
    let mut index = store.index().unwrap();
    let generation = index.next_generation();
    let payload = store.payload_dir(generation);
    let snapshot = payload.join(SNAPSHOT_NAME);
    private_dir(&payload);
    private_dir(&snapshot);
    fs::write(snapshot.join(MANIFEST_NAME), spec.encode()).unwrap();
    let manifest = TemplateManifest::read(&snapshot).unwrap();
    let key = TemplateKey::new(class, &manifest).unwrap();
    write_private(&payload.join(KEY_NAME), &key.to_bytes()).unwrap();
    let published = Generation {
        generation,
        key: key.identity(),
        class: class.identity().unwrap(),
        storage,
        created_at: manifest.created_at(),
        time: manifest.time().clone(),
        state_len: manifest.state_len(),
        memory_len: manifest.memory_len(),
    };
    index.generations.push(published.clone());
    index.next = StoreGeneration::new(generation.get().checked_add(1).unwrap());
    store.write_index(&index).unwrap();
    published
}

/// Publishes a capture of the class of [`ManifestSpec::template`], as [`publish_with`] does.
pub(super) fn publish(
    store: &Store,
    spec: &ManifestSpec,
    storage: StorageGeneration,
) -> Generation {
    publish_with(store, &class(), spec, storage)
}

/// Removes the generation of store generation `generation` from the index of `store`, under its
/// lock, as a retirement does.
pub(super) fn remove(store: &Store, generation: StoreGeneration) {
    let _lock = take_lock(store.dir(), None).unwrap();
    let mut index = store.index().unwrap();
    index
        .generations
        .retain(|indexed| indexed.generation != generation);
    store.write_index(&index).unwrap();
}

/// Returns the store's record `name` as JSON.
fn record(store: &Store, name: &str) -> Value {
    serde_json::from_slice(&fs::read(store.dir().join(name)).unwrap()).unwrap()
}

/// Replaces the store's record `name` with `value`.
fn rewrite(store: &Store, name: &str, value: &Value) {
    write_private(&store.dir().join(name), value.to_string().as_bytes()).unwrap();
}

/// Returns the sorted names of the entries of `dir`.
fn sorted_entries(dir: &Path) -> Vec<String> {
    let mut names = entries(dir).unwrap();
    names.sort();
    names
}

/// Returns the message of the cause of `error`.
pub(super) fn cause(error: &Error) -> String {
    std::error::Error::source(error)
        .map(ToString::to_string)
        .unwrap_or_default()
}

/// Links `link` to the directory `target`: a symbolic link on Unix, and a junction on Windows,
/// which any user may create.
pub(super) fn link_dir(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    crate::host::test_support::junction(link, target);
}

/// Links `link` to the file `target` with a symbolic link, and returns whether it could: Windows
/// lets only Developer Mode or a privilege create one.
pub(super) fn link_file(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_file(target, link);
    if linked.is_err() {
        eprintln!("skipped: this host does not let the test create a symbolic link");
    }
    linked.is_ok()
}

/// Removes the link `link` to a directory, which leaves its target as it is.
pub(super) fn unlink_dir(link: &Path) {
    #[cfg(unix)]
    fs::remove_file(link).unwrap();
    #[cfg(windows)]
    fs::remove_dir(link).unwrap();
}

#[test]
fn stores_are_created_opened_and_reopened() {
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path().join("templates");
    let error = Store::open(&dir).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
    let store = Store::create(&dir).unwrap();
    assert_eq!(store.dir(), dir);
    assert_eq!(
        sorted_entries(&dir),
        [
            GENERATIONS_NAME,
            INDEX_NAME,
            INDEX_LOCK_NAME,
            STORE_RECORD_NAME
        ]
    );
    assert!(entries(&dir.join(GENERATIONS_NAME)).unwrap().is_empty());
    let index = store.index().unwrap();
    assert!(index.generations().is_empty());
    assert_eq!(index.next_generation(), FIRST_GENERATION);
    // Opening it, creating it again, and reopening it find the same store, with its index.
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    assert_eq!(Store::open(&dir).unwrap(), store);
    assert_eq!(Store::create(&dir).unwrap(), store);
    let id = store.id();
    drop(store);
    let reopened = Store::open(&dir).unwrap();
    assert_eq!(reopened.id(), id);
    assert_eq!(reopened.index().unwrap().generations(), [published]);
    // Another store has an identity of its own.
    let other = Store::create(directory.path().join("other")).unwrap();
    assert_ne!(other.id(), id);
    // A directory without a store holds none.
    let empty = directory.path().join("empty");
    private_dir(&empty);
    let error = Store::open(&empty).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
    assert!(
        error.message().ends_with("holds no template store"),
        "{error}"
    );
    // A store needs its parent.
    let error = Store::create(directory.path().join("missing").join("templates")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
}

#[test]
fn a_creation_replaces_an_interrupted_one_and_refuses_foreign_entries() {
    let directory = tempfile::tempdir().unwrap();
    // What a creation leaves before it writes the identity record is replaced.
    let interrupted = directory.path().join("interrupted");
    private_dir(&interrupted);
    private_dir(&interrupted.join(GENERATIONS_NAME));
    for name in [
        INDEX_NAME,
        INDEX_LOCK_NAME,
        ".index.json.42.tmp",
        ".store.json.7.tmp",
    ] {
        fs::write(interrupted.join(name), b"{").unwrap();
    }
    let store = Store::create(&interrupted).unwrap();
    assert!(store.index().unwrap().generations().is_empty());
    assert_eq!(Store::open(&interrupted).unwrap(), store);
    // Anything else is refused, and kept as it is: other files, and the payloads of a store whose
    // identity record is gone.
    let foreign = directory.path().join("foreign");
    private_dir(&foreign);
    for name in ["notes.txt", ".index.json.1a.tmp", ".config.json.1.tmp"] {
        fs::write(foreign.join(name), b"").unwrap();
    }
    private_dir(&foreign.join(GENERATIONS_NAME));
    private_dir(&foreign.join(GENERATIONS_NAME).join("7"));
    let before = sorted_entries(&foreign);
    let error = Store::create(&foreign).unwrap_err();
    assert_eq!(error.code(), ErrorCode::ForeignFiles, "{error}");
    assert!(
        error.message().ends_with(
            "holds no template store, but entries that are not a store's: .config.json.1.tmp, \
             .index.json.1a.tmp, generations/7, notes.txt"
        ),
        "{error}"
    );
    assert_eq!(sorted_entries(&foreign), before);
    let error = Store::open(&foreign).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
}

#[test]
fn concurrent_creations_make_one_store() {
    /// The creations that race.
    const CREATIONS: usize = 8;
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path().join("templates");
    let barrier = Arc::new(Barrier::new(CREATIONS));
    let creations: Vec<_> = (0..CREATIONS)
        .map(|_| {
            let (dir, barrier) = (dir.clone(), Arc::clone(&barrier));
            thread::spawn(move || {
                barrier.wait();
                Store::create(&dir).unwrap().id()
            })
        })
        .collect();
    let ids: Vec<StoreId> = creations
        .into_iter()
        .map(|creation| creation.join().unwrap())
        .collect();
    assert!(ids.iter().all(|id| *id == ids[0]), "{ids:?}");
    assert_eq!(Store::open(&dir).unwrap().id(), ids[0]);
}

#[test]
fn a_creation_that_another_overtakes_opens_its_store() {
    let directory = tempfile::tempdir().unwrap();
    // A creation that finds no store, and then the entries of one that another creation made
    // meanwhile, even with a generation, opens that store.
    let dir = directory.path().join("templates");
    let mut overtaking = None;
    let store = Store::create_racing(dir.clone(), || {
        let store = Store::create(&dir).unwrap();
        publish(&store, &ManifestSpec::template(), storage(1));
        overtaking = Some(store);
    })
    .unwrap();
    assert_eq!(Some(store), overtaking);
    // Entries that no store explains are still refused.
    let foreign = directory.path().join("foreign");
    let error = Store::create_racing(foreign.clone(), || {
        fs::write(foreign.join("notes.txt"), b"").unwrap();
    })
    .unwrap_err();
    assert_eq!(error.code(), ErrorCode::ForeignFiles, "{error}");
}

#[test]
fn records_keep_their_json_form() {
    let (_directory, store) = store();
    let mut spec = ManifestSpec::template();
    // Windows keeps times in units of 100 ns.
    spec.created_at = (1_700_000_000, 123_456_700);
    spec.time().cut_to_anchor_ns = None;
    let published = publish(&store, &spec, storage(9));
    let manifest = TemplateManifest::parse(&spec.encode()).unwrap();
    let key = TemplateKey::new(&class(), &manifest).unwrap();
    let time = manifest.time();
    assert_eq!(
        record(&store, STORE_RECORD_NAME),
        json!({ "format": 1, "store": store.id().to_string() })
    );
    // The index holds no VM generation ID, which only the clones' guests know.
    assert_eq!(
        record(&store, INDEX_NAME),
        json!({
            "format": 1,
            "store": store.id().to_string(),
            "nextGeneration": 2,
            "generations": [{
                "generation": 1,
                "key": key.identity().to_string(),
                "class": class().identity().unwrap().to_string(),
                "storageGeneration": "09090909090909090909090909090909",
                "createdAt": { "seconds": 1_700_000_000, "nanos": 123_456_700 },
                "time": {
                    "tscFrequencyHz": time.tsc_frequency_hz,
                    "tscTolerancePpm": time.tsc_tolerance_ppm,
                    "apicFrequencyHz": time.apic_frequency_hz,
                    "captureTsc": time.capture_tsc,
                    "captureUtcNs": time.capture_utc_ns,
                    "captureMonotonicNs": time.capture_monotonic_ns,
                    "hostClock": "linux-boottime",
                    "hostId": hex(&time.host_id),
                    "bootId": time.boot_id.to_string(),
                    "lineage": time.lineage,
                    "cutToAnchorNs": null,
                },
                "stateLen": manifest.state_len(),
                "memoryLen": manifest.memory_len(),
            }],
        })
    );
    // Each generation keeps its own metadata, which its manifest recorded, and its key.
    let index = Store::open(store.dir()).unwrap().index().unwrap();
    assert_eq!(index.generations(), std::slice::from_ref(&published));
    assert_eq!(index.get(FIRST_GENERATION), Some(&published));
    assert_eq!(published.generation(), FIRST_GENERATION);
    assert_eq!(published.key(), key.identity());
    assert_eq!(published.class(), class().identity().unwrap());
    assert_eq!(published.storage_generation(), storage(9));
    assert_eq!(published.created_at(), manifest.created_at());
    assert_eq!(published.time(), manifest.time());
    assert_eq!(published.state_len(), manifest.state_len());
    assert_eq!(published.memory_len(), manifest.memory_len());
    assert_eq!(store.read_key(&published).unwrap(), key);
    assert_eq!(
        store.snapshot_dir(&published).unwrap(),
        store
            .dir()
            .join(GENERATIONS_NAME)
            .join("1")
            .join(SNAPSHOT_NAME)
    );
}

#[test]
fn records_of_other_formats_fail_clearly() {
    let (_directory, store) = store();
    publish(&store, &ManifestSpec::template(), storage(1));
    let valid = record(&store, INDEX_NAME);
    for format in [0, 2] {
        let mut index = valid.clone();
        index["format"] = format.into();
        rewrite(&store, INDEX_NAME, &index);
        let error = store.index().unwrap_err();
        assert_eq!(error.code(), ErrorCode::FormatUnsupported, "{error}");
        assert!(
            error.message().contains(&format!(
                "has the unsupported format {format}; this build reads format 1 only and \
                 migrates no other"
            )),
            "{error}"
        );
    }
    let mut index = valid.clone();
    index.as_object_mut().unwrap().remove("format");
    rewrite(&store, INDEX_NAME, &index);
    let error = store.index().unwrap_err();
    assert_eq!(error.code(), ErrorCode::Io, "{error}");
    assert!(error.message().ends_with("it has no format"), "{error}");
    rewrite(&store, INDEX_NAME, &valid);
    assert_eq!(store.index().unwrap().generations().len(), 1);
    // A store whose identity record is of another format is opened by no build of this one.
    rewrite(
        &store,
        STORE_RECORD_NAME,
        &json!({ "format": 2, "store": store.id().to_string() }),
    );
    for error in [
        Store::open(store.dir()).unwrap_err(),
        Store::create(store.dir()).unwrap_err(),
    ] {
        assert_eq!(error.code(), ErrorCode::FormatUnsupported, "{error}");
    }
}

#[test]
fn malformed_indexes_and_those_of_other_stores_are_refused() {
    let (_directory, store) = store();
    publish(&store, &ManifestSpec::template(), storage(1));
    publish(&store, &ManifestSpec::template(), storage(2));
    let valid = record(&store, INDEX_NAME);
    let cases: [(&str, Change); 11] = [
        ("its next store generation is 0", |index| {
            index["nextGeneration"] = 0.into();
        }),
        (
            "it lists store generation 2, which its next store generation, 2, does not follow",
            |index| index["nextGeneration"] = 2.into(),
        ),
        (
            "it lists store generation 1 out of order, or twice",
            |index| {
                index["generations"][1]["generation"] = 1.into();
            },
        ),
        (
            "it lists store generation 0, which no generation has",
            |index| index["generations"][0]["generation"] = 0.into(),
        ),
        ("the key of store generation 1 is invalid", |index| {
            index["generations"][0]["key"] = "00".into();
        }),
        ("the class of store generation 1 is invalid", |index| {
            index["generations"][0]["class"] = "".into();
        }),
        (
            "the storage generation of store generation 1 is invalid",
            |index| index["generations"][0]["storageGeneration"] = "0".repeat(32).into(),
        ),
        (
            "the creation time of store generation 1 is invalid",
            |index| {
                index["generations"][0]["createdAt"]["nanos"] = 1_000_000_000.into();
            },
        ),
        (
            "the time contract of store generation 1 has the unknown host clock \"tsc\"",
            |index| index["generations"][0]["time"]["hostClock"] = "tsc".into(),
        ),
        (
            "the time contract of store generation 1 has the invalid host identity \"01\"",
            |index| index["generations"][0]["time"]["hostId"] = "01".into(),
        ),
        (
            "it lists 1025 generations, more than the 1024 that a store holds",
            |index| {
                let entry = index["generations"][0].clone();
                index["generations"] = (1..=MAX_GENERATIONS + 1)
                    .map(|generation| {
                        let mut entry = entry.clone();
                        entry["generation"] = generation.into();
                        entry
                    })
                    .collect();
                index["nextGeneration"] = (MAX_GENERATIONS + 2).into();
            },
        ),
    ];
    for (problem, change) in cases {
        let mut index = valid.clone();
        change(&mut index);
        rewrite(&store, INDEX_NAME, &index);
        let error = store.index().unwrap_err();
        assert_eq!(error.code(), ErrorCode::Io, "{error}");
        assert!(
            error
                .message()
                .contains(&format!("is malformed: {problem}")),
            "{problem}: {error}"
        );
    }
    // Fields that the format does not have are refused.
    let mut index = valid.clone();
    index["generations"][0]["generationId"] = "00".repeat(16).into();
    rewrite(&store, INDEX_NAME, &index);
    let error = store.index().unwrap_err();
    assert!(error.message().ends_with("is malformed"), "{error}");
    assert!(
        cause(&error).starts_with("unknown field `generationId`"),
        "{error}"
    );
    // So is the index of another store.
    let mut index = valid.clone();
    index["store"] = "07".repeat(16).into();
    rewrite(&store, INDEX_NAME, &index);
    let error = store.index().unwrap_err();
    assert!(
        error.message().ends_with(&format!(
            "belongs to the template store {}, not to this one, {}",
            "07".repeat(16),
            store.id()
        )),
        "{error}"
    );
    // And one longer than an index may be, before anything of it is parsed.
    let path = store.dir().join(INDEX_NAME);
    write_private(&path, &vec![b' '; MAX_INDEX_LEN as usize + 1]).unwrap();
    let error = store.index().unwrap_err();
    assert_eq!(
        cause(&error),
        format!(
            "it holds more than the {MAX_INDEX_LEN} bytes that the index of the template store \
             may hold"
        )
    );
    // A missing index is an error of the store, not an empty index.
    fs::remove_file(&path).unwrap();
    let error = store.index().unwrap_err();
    assert_eq!(error.code(), ErrorCode::Io, "{error}");
    assert!(error.message().ends_with("has no index"), "{error}");
    rewrite(&store, INDEX_NAME, &valid);
    assert_eq!(store.index().unwrap().generations().len(), 2);
}

#[test]
fn the_four_namespaces_stay_apart() {
    let (_directory, store) = store();
    // Store generations follow the order of publication alone: neither the time lineage counters,
    // nor the storage generations, which captures may share, and which may look like a store
    // generation.
    let one = StorageGeneration::from_bytes(1u128.to_be_bytes()).unwrap();
    let mut published = Vec::new();
    for (lineage, storage) in [(7, one), (7, one), (1, storage(0xff))] {
        let mut spec = ManifestSpec::template();
        spec.time().lineage = lineage;
        let generation = publish(&store, &spec, storage);
        published.push((
            generation.generation().get().get(),
            generation.time().lineage,
            generation.storage_generation(),
        ));
    }
    assert_eq!(published, [(1, 7, one), (2, 7, one), (3, 1, storage(0xff))]);
    // A store generation is never assigned again, even once its generation is gone.
    let third = StoreGeneration::new(NonZeroU64::new(3).unwrap());
    remove(&store, third);
    let index = store.index().unwrap();
    assert_eq!(index.get(third), None);
    assert_eq!(index.next_generation().get().get(), 4);
    let fourth = publish(&store, &ManifestSpec::template(), one);
    assert_eq!(fourth.generation().get().get(), 4);
    // The generations of one key, whatever their lineage counters and storage generations.
    let of_key: Vec<u64> = store
        .index()
        .unwrap()
        .of_key(fourth.key())
        .map(|generation| generation.generation().get().get())
        .collect();
    assert_eq!(of_key, [1, 2, 4]);
    // The text form of each names nothing of another's.
    assert_eq!(one.to_string(), "00000000000000000000000000000001");
    assert_eq!(fourth.generation().to_string(), "4");
    assert!("1".parse::<StorageGeneration>().is_err());
    assert!("4".parse::<StorageGeneration>().is_err());
    assert_eq!(StorageGeneration::from_bytes([0; ID_LEN]), None);
    assert!("0".repeat(32).parse::<StorageGeneration>().is_err());
    assert_eq!(
        "00000000000000000000000000000001"
            .parse::<StorageGeneration>()
            .unwrap(),
        one
    );
    assert_eq!(
        "FF".repeat(16).parse::<StorageGeneration>().unwrap(),
        storage(0xff)
    );
    assert_eq!(format!("{one:?}"), format!("StorageGeneration({one})"));
    let drawn = StorageGeneration::generate().unwrap();
    assert_ne!(drawn, StorageGeneration::generate().unwrap());
    // A store's identity is none of them either.
    let id = store.id();
    assert_eq!(id.to_string().parse::<StoreId>().unwrap(), id);
    assert_eq!(
        id.to_string().to_uppercase().parse::<StoreId>().unwrap(),
        id
    );
    assert_eq!(format!("{id:?}"), format!("StoreId({id})"));
    assert!("1".parse::<StoreId>().is_err());
}

#[test]
fn payloads_are_checked_as_they_are_read() {
    let (_directory, store) = store();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let payload = store.payload_dir(published.generation());
    let key = payload.join(KEY_NAME);
    let bytes = fs::read(&key).unwrap();
    // A key that is not a key's canonical encoding is refused as such.
    write_private(&key, &[2, 0]).unwrap();
    let error = store.read_key(&published).unwrap_err();
    assert_eq!(error.code(), ErrorCode::FormatUnsupported, "{error}");
    assert!(
        error
            .message()
            .starts_with(&format!("cannot read {}: ", key.display())),
        "{error}"
    );
    write_private(&key, &bytes[..bytes.len() - 1]).unwrap();
    let error = store.read_key(&published).unwrap_err();
    assert_eq!(error.code(), ErrorCode::Io, "{error}");
    write_private(&key, &vec![0; TemplateKey::MAX_LEN + 1]).unwrap();
    let error = store.read_key(&published).unwrap_err();
    assert_eq!(
        cause(&error),
        format!(
            "it holds more than the {} bytes that the template key may hold",
            TemplateKey::MAX_LEN
        )
    );
    // A missing key, snapshot, or payload, which a crash may lose, is missing.
    fs::remove_file(&key).unwrap();
    let error = store.read_key(&published).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
    fs::remove_dir_all(payload.join(SNAPSHOT_NAME)).unwrap();
    let error = store.snapshot_dir(&published).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
    fs::remove_dir_all(&payload).unwrap();
    for error in [
        store.snapshot_dir(&published).unwrap_err(),
        store.read_key(&published).unwrap_err(),
    ] {
        assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
    }
}

#[test]
fn the_index_lock_excludes_other_holders() {
    let (_directory, store) = store();
    let held = take_lock(store.dir(), None).unwrap();
    let started = Instant::now();
    let deadline = Some(Instant::now() + Duration::from_millis(100));
    let error = take_lock(store.dir(), deadline).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TimedOut, "{error}");
    assert!(error.message().ends_with("is still in progress"), "{error}");
    assert!(started.elapsed() >= Duration::from_millis(100));
    // A creation takes the lock too, and waits for its holder to release it.
    let (sender, receiver) = mpsc::channel();
    let dir = store.dir().to_owned();
    let creation = thread::spawn(move || {
        let created = Store::create(&dir).map(|store| store.id());
        sender.send(()).unwrap();
        created
    });
    assert!(receiver.recv_timeout(Duration::from_millis(200)).is_err());
    drop(held);
    receiver.recv_timeout(Duration::from_secs(30)).unwrap();
    assert_eq!(creation.join().unwrap().unwrap(), store.id());
}

#[test]
fn links_are_refused() {
    let (directory, store) = store();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    // A path that a separator ends names the store's own directory.
    assert_eq!(Store::open(store.dir().join("")).unwrap(), store);
    assert_eq!(Store::create(store.dir().join("")).unwrap(), store);
    // A link to a store is no store, however its path ends: Linux follows a link at a name that a
    // separator ends, even when it follows none at the final name.
    let link = directory.path().join("link");
    link_dir(store.dir(), &link);
    for path in [link.clone(), link.join(""), link.join(".")] {
        for error in [
            Store::create(&path).unwrap_err(),
            Store::open(&path).unwrap_err(),
        ] {
            assert_eq!(error.code(), ErrorCode::Io, "{}: {error}", path.display());
            assert!(
                error.message().ends_with("is not a plain directory"),
                "{}: {error}",
                path.display()
            );
        }
    }
    // Nor is a link to the generations, to a payload, or to a snapshot followed.
    let payload = store.payload_dir(published.generation());
    for path in [
        store.dir().join(GENERATIONS_NAME),
        payload.clone(),
        payload.join(SNAPSHOT_NAME),
    ] {
        let moved = path.with_extension("moved");
        fs::rename(&path, &moved).unwrap();
        link_dir(&moved, &path);
        let mut errors = vec![store.snapshot_dir(&published).unwrap_err()];
        if path != payload.join(SNAPSHOT_NAME) {
            errors.push(store.read_key(&published).unwrap_err());
        }
        if path.ends_with(GENERATIONS_NAME) {
            errors.push(Store::open(store.dir()).unwrap_err());
        }
        for error in errors {
            assert!(
                error.message().ends_with("is not a plain directory"),
                "{}: {error}",
                path.display()
            );
        }
        unlink_dir(&path);
        fs::rename(&moved, &path).unwrap();
    }
    assert_eq!(
        store.read_key(&published).unwrap().identity(),
        published.key()
    );
    // Nor is a link to a record, or to a key.
    for path in [
        store.dir().join(STORE_RECORD_NAME),
        store.dir().join(INDEX_NAME),
        payload.join(KEY_NAME),
    ] {
        let moved = path.with_extension("moved");
        fs::rename(&path, &moved).unwrap();
        if !link_file(&moved, &path) {
            fs::rename(&moved, &path).unwrap();
            return;
        }
        let error = if path.ends_with(STORE_RECORD_NAME) {
            Store::open(store.dir()).unwrap_err()
        } else if path.ends_with(INDEX_NAME) {
            store.index().unwrap_err()
        } else {
            store.read_key(&published).unwrap_err()
        };
        assert_eq!(
            cause(&error),
            "it is a link, or not a regular file",
            "{}: {error}",
            path.display()
        );
        fs::remove_file(&path).unwrap();
        fs::rename(&moved, &path).unwrap();
    }
    // Nor a link to the lock file.
    let lock = store.dir().join(INDEX_LOCK_NAME);
    let moved = lock.with_extension("moved");
    fs::rename(&lock, &moved).unwrap();
    assert!(link_file(&moved, &lock));
    let error = take_lock(store.dir(), None).unwrap_err();
    assert_eq!(cause(&error), "is not a regular file", "{error}");
}

#[cfg(unix)]
#[test]
fn records_that_are_not_regular_files_fail_at_once() {
    use nix::sys::stat::Mode;

    let (_directory, store) = store();
    for name in [STORE_RECORD_NAME, INDEX_NAME, INDEX_LOCK_NAME] {
        let path = store.dir().join(name);
        let moved = path.with_extension("moved");
        fs::rename(&path, &moved).unwrap();
        nix::unistd::mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        // A read that waited for a writer would never end, so it runs on a thread that the test
        // can leave behind.
        let (sender, receiver) = mpsc::channel();
        let opened = store.clone();
        thread::spawn(move || {
            let result = match name {
                STORE_RECORD_NAME => Store::open(opened.dir()).map(drop),
                INDEX_NAME => opened.index().map(drop),
                _ => take_lock(opened.dir(), None).map(drop),
            };
            sender.send(result).unwrap();
        });
        let error = receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("the read does not wait for a writer")
            .unwrap_err();
        assert!(
            [
                "it is a link, or not a regular file",
                "is not a regular file"
            ]
            .contains(&cause(&error).as_str()),
            "{name}: {error}"
        );
        fs::remove_file(&path).unwrap();
        fs::rename(&moved, &path).unwrap();
    }
}

#[cfg(target_os = "linux")]
#[test]
fn the_store_is_private_to_the_current_user() {
    use std::os::unix::fs::PermissionsExt;

    let (directory, store) = store();
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o7777;
    let set_mode = |path: &Path, mode: u32| {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    };
    // The store's directories and records are the current user's alone.
    for path in [store.dir().to_owned(), store.dir().join(GENERATIONS_NAME)] {
        assert_eq!(mode(&path), 0o700, "{}", path.display());
    }
    for name in [STORE_RECORD_NAME, INDEX_NAME, INDEX_LOCK_NAME] {
        assert_eq!(mode(&store.dir().join(name)), 0o600, "{name}");
    }
    // A directory that others could access is refused, and left as it is, rather than
    // restricted: another user could hold its files open.
    let shared = directory.path().join("shared");
    fs::create_dir(&shared).unwrap();
    for shared_mode in [0o750, 0o705, 0o755, 0o777] {
        set_mode(&shared, shared_mode);
        for error in [
            Store::create(&shared).unwrap_err(),
            Store::open(&shared).unwrap_err(),
        ] {
            let expected = format!("is accessible to other users (mode {shared_mode:03o})");
            assert!(error.message().ends_with(&expected), "{error}");
        }
        assert_eq!(mode(&shared), shared_mode);
    }
    assert!(entries(&shared).unwrap().is_empty());
    // So are a store whose generations others could access, and its payloads.
    let generations = store.dir().join(GENERATIONS_NAME);
    set_mode(&generations, 0o750);
    let error = Store::open(store.dir()).unwrap_err();
    assert!(
        error
            .message()
            .ends_with("is accessible to other users (mode 750)"),
        "{error}"
    );
    set_mode(&generations, 0o700);
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let snapshot = store
        .payload_dir(published.generation())
        .join(SNAPSHOT_NAME);
    set_mode(&snapshot, 0o755);
    let error = store.snapshot_dir(&published).unwrap_err();
    assert!(
        error
            .message()
            .ends_with("is accessible to other users (mode 755)"),
        "{error}"
    );
    // A store created under a setgid directory drops the bit for mode 0700, and an existing
    // directory that grants no one else access is private, whatever else its mode holds.
    let setgid = directory.path().join("setgid");
    fs::create_dir(&setgid).unwrap();
    set_mode(&setgid, 0o2700);
    let nested = Store::create(setgid.join("templates")).unwrap();
    assert_eq!(mode(nested.dir()), 0o700);
    let existing = directory.path().join("existing");
    fs::create_dir(&existing).unwrap();
    set_mode(&existing, 0o2700);
    Store::create(&existing).unwrap();
    assert_eq!(mode(&existing), 0o2700);
}

#[cfg(target_os = "linux")]
#[test]
fn private_directories_belong_to_the_current_user_and_grant_others_nothing() {
    use nix::sys::stat::Mode;
    use nix::unistd::Uid;

    // The check takes the owner and the current user as user IDs, and the permission bits of the
    // directory's mode, without the bits of its type, as the store reads them from its metadata.
    let owner_only_problem = |uid, mode, euid| {
        platform::owner_only_problem(
            Uid::from_raw(uid),
            Mode::from_bits_truncate(mode),
            Uid::from_raw(euid),
        )
    };
    // Another user's directory is refused whatever its mode, since its owner may change it.
    assert_eq!(
        owner_only_problem(1001, 0o40700, 1000),
        Some("belongs to user 1001, not to the current user 1000".to_owned())
    );
    for mode in [0o40700, 0o42700, 0o41700, 0o40500, 0o40300] {
        assert_eq!(owner_only_problem(1000, mode, 1000), None, "{mode:o}");
    }
    for mode in [0o40750, 0o40705, 0o40770, 0o40701, 0o40710] {
        let problem = owner_only_problem(1000, mode, 1000).unwrap();
        assert!(
            problem.starts_with("is accessible to other users"),
            "{problem}"
        );
    }
}

#[cfg(windows)]
#[test]
fn the_store_is_private_to_the_current_user() {
    let directory = tempfile::tempdir().unwrap();
    platform::set_access(directory.path(), "D:(A;OICI;FA;;;WD)").unwrap();
    // A directory that every user may change holds no store, nor does one created in it.
    let error = Store::open(directory.path()).unwrap_err();
    assert!(error.message().ends_with("allows other users"), "{error}");
    let shared = directory.path().join("shared");
    fs::create_dir(&shared).unwrap();
    let error = Store::create(&shared).unwrap_err();
    assert!(error.message().ends_with("allows other users"), "{error}");
    assert!(entries(&shared).unwrap().is_empty());
    // Nor does one that allows its owner alone, but passes nothing on to the files created in it,
    // which would take their creator's default list.
    platform::set_access(&shared, "D:P(A;;FA;;;OW)").unwrap();
    for error in [
        Store::create(&shared).unwrap_err(),
        Store::open(&shared).unwrap_err(),
    ] {
        assert!(
            error
                .message()
                .ends_with("passes no access control entries on to the files created in it"),
            "{error}"
        );
    }
    // A store created there allows the current user alone, and so do its records.
    let store = Store::create(directory.path().join("templates")).unwrap();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let payload = store.payload_dir(published.generation());
    let problem = |path: &Path| {
        let file = File::open(path).unwrap();
        platform::private_file_problem(&file).unwrap()
    };
    for path in [
        store.dir().join(STORE_RECORD_NAME),
        store.dir().join(INDEX_NAME),
        store.dir().join(INDEX_LOCK_NAME),
        payload.join(KEY_NAME),
    ] {
        assert_eq!(problem(&path), None, "{}", path.display());
    }
    // A payload that every user may change is refused.
    let snapshot = payload.join(SNAPSHOT_NAME);
    platform::set_access(&snapshot, "D:(A;OICI;FA;;;WD)").unwrap();
    let error = store.snapshot_dir(&published).unwrap_err();
    assert!(error.message().ends_with("allows other users"), "{error}");
    // So is a record or a key whose own list allows others, who could have written it, whatever
    // the store's directory allows, and a lock file that others could hold.
    for name in [KEY_NAME, INDEX_NAME, INDEX_LOCK_NAME, STORE_RECORD_NAME] {
        let path = if name == KEY_NAME {
            payload.join(name)
        } else {
            store.dir().join(name)
        };
        platform::set_access(&path, "D:P(A;;FA;;;WD)").unwrap();
        let error = match name {
            KEY_NAME => store.read_key(&published).unwrap_err(),
            INDEX_NAME => store.index().unwrap_err(),
            INDEX_LOCK_NAME => take_lock(store.dir(), None).unwrap_err(),
            _ => Store::open(store.dir()).unwrap_err(),
        };
        assert!(
            error.message().ends_with("allows other users"),
            "{name}: {error}"
        );
    }
}

#[cfg(windows)]
#[test]
fn snapshots_that_openvmm_published_are_private_without_entries_for_their_files() {
    let (_directory, store) = store();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let payload = store.payload_dir(published.generation());
    // OpenVMM gives a snapshot's directory, and each of its files, a protected list of their own,
    // which allows SYSTEM and the owner, and passes nothing on to the files created in it.
    let openvmm = "D:P(A;;FA;;;SY)(A;;FA;;;OW)";
    let snapshot = payload.join(SNAPSHOT_NAME);
    platform::set_access(&snapshot.join(MANIFEST_NAME), openvmm).unwrap();
    platform::set_access(&snapshot, openvmm).unwrap();
    assert_eq!(store.snapshot_dir(&published).unwrap(), snapshot);
    // The host's own directories still pass entries on to the files created in them.
    platform::set_access(&payload, openvmm).unwrap();
    let error = store.snapshot_dir(&published).unwrap_err();
    assert!(
        error
            .message()
            .ends_with("passes no access control entries on to the files created in it"),
        "{error}"
    );
    // A snapshot whose list allows anyone else is refused.
    platform::set_access(&payload, "D:P(A;OICI;FA;;;OW)").unwrap();
    platform::set_access(&snapshot, "D:P(A;;FA;;;SY)(A;;FA;;;OW)(A;;FR;;;WD)").unwrap();
    let error = store.snapshot_dir(&published).unwrap_err();
    assert!(error.message().ends_with("allows other users"), "{error}");
}
