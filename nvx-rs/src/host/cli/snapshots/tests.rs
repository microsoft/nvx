use std::fs;
use std::path::Path;

use super::*;
use crate::host::snapshot::tests::SnapshotFixture;
use crate::host::snapshot::{DESCRIPTION_FILE, KEY_FILE, MANIFEST_NAME, OPENVMM_DIR};
use crate::host::tests::released;

fn name(value: &str) -> Name {
    Name::try_from(value.to_owned()).unwrap()
}

fn staged(locked: &LockedSnapshot<'_>) -> StateDir {
    let fixture = SnapshotFixture::new();
    let staged = locked.stage().unwrap();
    for entry in [KEY_FILE, DESCRIPTION_FILE, OPENVMM_DIR] {
        fs::rename(fixture.dir.join(entry), staged.dir().join(entry)).unwrap();
    }
    staged
}

fn link_directory(link: &Path, target: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    crate::host::tests::junction(link, target);
}

#[test]
fn names_are_bounded_portable_components_with_distinct_mappings() {
    for invalid in [
        "",
        ".",
        "..",
        ".hidden",
        "../escape",
        "a/b",
        "a\\b",
        "a:b",
        "a b",
        "a\nb",
        "snapshot\u{e9}",
    ] {
        assert!(Name::try_from(invalid.to_owned()).is_err(), "{invalid:?}");
    }
    assert!(Name::try_from("a".repeat(65)).is_err());
    assert!(Name::try_from("a".repeat(64)).is_ok());
    let root = tempfile::tempdir().unwrap();
    let store = Store::create(&root.path().join("store")).unwrap();
    let mut mapped = Vec::new();
    for valid in ["a", "CON", "con.", "A", "name.1-2_3"] {
        let held = store.lock(&name(valid), Deadline::after(0.0)).unwrap();
        assert_eq!(held.dir.parent(), Some(store.entries.dir()));
        assert!(!held.dir.exists());
        assert!(!held._lock.dir().starts_with(&held.dir));
        mapped.push(
            held.dir
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_ascii_lowercase(),
        );
    }
    mapped.sort();
    mapped.dedup();
    assert_eq!(mapped.len(), 5);
}

#[test]
fn publication_is_strict_whole_and_cannot_replace_or_move_a_published_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::create(&root.path().join("store")).unwrap();
    let held = store.lock(&name("first"), Deadline::after(0.0)).unwrap();
    let pending = staged(&held);
    fs::write(pending.dir().join("log"), b"not a snapshot entry").unwrap();
    assert!(held.publish(pending.clone()).is_err());
    assert!(pending.dir().exists());
    assert!(!held.dir.exists());
    fs::remove_file(pending.dir().join("log")).unwrap();
    let expected = Snapshot::open(pending.dir()).unwrap();
    let key = fs::read(pending.dir().join(KEY_FILE)).unwrap();
    let published = held.publish(pending.clone()).unwrap();
    assert!(!pending.dir().exists());
    assert_eq!(published.key(), expected.key());
    assert_eq!(published.manifest(), expected.manifest());
    let mut entries: Vec<_> = fs::read_dir(&held.dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    entries.sort();
    assert_eq!(entries, [KEY_FILE, OPENVMM_DIR, DESCRIPTION_FILE]);
    let files = [
        held.dir.join(KEY_FILE),
        held.dir.join(DESCRIPTION_FILE),
        held.dir.join(OPENVMM_DIR).join(MANIFEST_NAME),
        held.dir.join(OPENVMM_DIR).join("memory.bin"),
        held.dir.join(OPENVMM_DIR).join("state.bin"),
    ];
    let unchanged = || {
        files
            .iter()
            .map(|path| {
                let metadata = fs::metadata(path).unwrap();
                (
                    platform::file_id(path).unwrap(),
                    metadata.len(),
                    metadata.modified().unwrap(),
                )
            })
            .collect::<Vec<_>>()
    };
    let before = unchanged();
    let replacement = staged(&held);
    assert!(held.publish(replacement.clone()).is_err());
    assert!(replacement.dir().exists());
    assert!(held.publish(StateDir::open(&held.dir).unwrap()).is_err());
    assert_eq!(fs::read(held.dir.join(KEY_FILE)).unwrap(), key);
    assert_eq!(
        fs::read(held.dir.join(DESCRIPTION_FILE)).unwrap(),
        b"not JSON"
    );
    assert_eq!(held.open().unwrap().key(), published.key());
    assert_eq!(unchanged(), before);
}

#[test]
fn invalid_or_linked_directories_never_publish_or_open() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::create(&root.path().join("store")).unwrap();
    let held = store.lock(&name("first"), Deadline::after(0.0)).unwrap();
    let pending = staged(&held);
    crate::host::store::files::write_private(&pending.dir().join(KEY_FILE), b"bad key").unwrap();
    assert!(held.publish(pending.clone()).is_err());
    assert!(!held.dir.exists());
    let valid = staged(&held);
    let expected = Snapshot::open(valid.dir()).unwrap();
    let identity = platform::file_id(valid.dir()).unwrap();
    let key = fs::read(valid.dir().join(KEY_FILE)).unwrap();
    let linked = held.stage().unwrap();
    fs::remove_dir(linked.dir()).unwrap();
    link_directory(linked.dir(), valid.dir());
    assert!(held.publish(linked).is_err());
    assert!(!held.dir.exists());
    link_directory(&held.dir, valid.dir());
    assert!(held.open().is_err());
    assert!(held.publish(staged(&held)).is_err());
    assert_eq!(platform::file_id(valid.dir()).unwrap(), identity);
    assert_eq!(fs::read(valid.dir().join(KEY_FILE)).unwrap(), key);
    assert_eq!(Snapshot::open(valid.dir()).unwrap().key(), expected.key());
    let alias = root.path().join("alias");
    link_directory(&alias, store.root.dir());
    assert!(Store::create(&alias).is_err());
}

#[test]
fn exclusive_snapshot_locks_wait_and_other_names_remain_independent() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::create(&root.path().join("store")).unwrap();
    let first = store.lock(&name("first"), Deadline::after(0.0)).unwrap();
    let other_handle = Store::create(store.root.dir()).unwrap();
    assert!(matches!(
        other_handle.lock(&name("first"), Deadline::after(0.0)),
        Err(Exit::Error {
            code: ErrorCode::SandboxBusy,
            ..
        })
    ));
    let second = store.lock(&name("second"), Deadline::after(0.0)).unwrap();
    assert_ne!(first._lock.dir(), second._lock.dir());
    drop(first);
    assert!(released(|| {
        other_handle
            .lock(&name("first"), Deadline::after(0.0))
            .is_ok()
    }));
    assert_eq!(fs::read_dir(store.entries.dir()).unwrap().count(), 0);
}

#[cfg(target_os = "linux")]
#[test]
fn shared_directories_and_linked_locks_are_refused_without_restricting_them() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let root = tempfile::tempdir().unwrap();
    let shared = root.path().join("shared");
    fs::create_dir(&shared).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Store::create(&shared).is_err());
    assert_eq!(
        fs::metadata(&shared).unwrap().permissions().mode() & 0o777,
        0o755
    );
    let store = Store::create(&root.path().join("store")).unwrap();
    let held = store.lock(&name("first"), Deadline::after(0.0)).unwrap();
    let pending = staged(&held);
    fs::set_permissions(pending.dir(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(held.publish(pending).is_err());
    let lock = held._lock.path(super::super::store::StateFile::Lock);
    drop(held);
    fs::remove_file(&lock).unwrap();
    let target = root.path().join("target");
    fs::write(&target, b"unchanged").unwrap();
    symlink(&target, &lock).unwrap();
    assert!(store.lock(&name("first"), Deadline::after(0.0)).is_err());
    assert_eq!(fs::read(&target).unwrap(), b"unchanged");
}

#[cfg(windows)]
#[test]
fn shared_directory_acls_are_refused_before_publication() {
    use std::process::{Command, Stdio};

    let allow_everyone = |path: &Path| {
        let status = crate::host::tests::spawn_locked(
            Command::new("icacls")
                .arg(path)
                .args(["/grant", "*S-1-1-0:(OI)(CI)(F)"])
                .stdout(Stdio::null()),
        )
        .unwrap()
        .wait()
        .unwrap();
        assert!(status.success());
    };
    let root = tempfile::tempdir().unwrap();
    let shared = StateDir::create(&root.path().join("shared")).unwrap();
    allow_everyone(shared.dir());
    assert!(Store::create(shared.dir()).is_err());
    assert!(platform::check_store_dir(shared.dir()).unwrap().is_some());
    let store = Store::create(&root.path().join("store")).unwrap();
    let held = store.lock(&name("first"), Deadline::after(0.0)).unwrap();
    let pending = staged(&held);
    allow_everyone(&pending.dir().join(OPENVMM_DIR));
    assert!(held.publish(pending.clone()).is_err());
    assert!(pending.dir().exists());
    assert!(!held.dir.exists());
}
