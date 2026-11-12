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
    crate::host::platform::write_private(&pending.dir().join(KEY_FILE), b"bad key").unwrap();
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

/// Parses the `snapshot` command line `arguments`.
fn command(arguments: &[&str]) -> Result<(Command, Vec<String>)> {
    parse(
        &arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect::<Vec<_>>(),
    )
}

#[test]
fn the_snapshot_parser_selects_capture_or_a_restore_with_the_options_of_a_provision() {
    let Ok((Command::Capture(_), extras)) = command(&["capture", "name", "--store-dir", "store"])
    else {
        panic!("capture parses");
    };
    assert!(extras.is_empty());
    let layer = "distro,image.gpt,11111111-1111-1111-1111-111111111111";
    let Ok((Command::Restore(options), extras)) = command(&[
        "restore",
        "name",
        "--store-dir",
        "store",
        "--state-dir",
        "state",
        "--layer",
        layer,
        "--layer-format",
        "gpt",
        "--upper",
        "ram",
        "--ram-upper-percent",
        "25",
        "--entrypoint",
        "/bin/echo",
        "--arg",
        "restored",
        "--workload-user",
        "1000:1001",
    ]) else {
        panic!("restore parses");
    };
    assert!(extras.is_empty());
    assert_eq!(options.name, Some(name("name")));
    assert_eq!(options.store_dir, "store");
    let sandbox = options.sandbox;
    assert_eq!(sandbox.operation, SandboxOperation::Provision);
    assert_eq!(sandbox.state_dir.as_deref(), Some("state"));
    assert_eq!(sandbox.layers.len(), 1);
    // The restored sandbox's typed record takes provision's image format and RAM upper cap.
    assert_eq!(
        sandbox.layer_format,
        Some(crate::host::sandbox::distro::DistroFormat::Gpt)
    );
    assert_eq!(sandbox.upper, Some(crate::protocol::root::RootUpper::Ram));
    assert_eq!(sandbox.ram_upper_percent, Some(25));
    assert_eq!(
        (sandbox.entrypoint.as_str(), sandbox.args.as_slice()),
        ("/bin/echo", ["restored".to_owned()].as_slice())
    );
    assert_eq!(sandbox.workload_user, (1000, 1001));
    // A restore needs its snapshot, its store, and its sandbox's state directory.
    for arguments in [
        &["restore", "name", "--state-dir", "state"][..],
        &["restore", "name", "--store-dir", "store"],
        &["restore", "--store-dir", "store", "--state-dir", "state"],
        &[
            "restore",
            "../escape",
            "--store-dir",
            "store",
            "--state-dir",
            "state",
        ],
        &["bogus"],
    ] {
        let Err(Exit::Usage { code, .. }) = command(arguments) else {
            panic!("{arguments:?} is a usage error");
        };
        let expected = if arguments.contains(&"../escape") || arguments == ["bogus"] {
            ErrorCode::ArgumentInvalid
        } else {
            ErrorCode::ArgumentMissing
        };
        assert_eq!(code, expected, "{arguments:?}");
    }
    // A restore's sandbox runs no execution and boots nothing cold, so it takes none of their
    // options.
    for option in NOT_RESTORED {
        let Ok((_, extras)) = command(&[
            "restore",
            "name",
            "--store-dir",
            "store",
            "--state-dir",
            "state",
            option,
        ]) else {
            panic!("{option} parses as an unrecognized argument");
        };
        assert_eq!(extras, [option], "{option}");
    }
    for arguments in [
        &["--help"][..],
        &["restore", "--help"],
        &["capture", "--help"],
    ] {
        assert!(
            matches!(command(arguments), Err(Exit::Help(_))),
            "{arguments:?}"
        );
    }
    RESTORE_SYNTAX.assert_lists(&restore_options());
}

#[test]
fn a_restore_refuses_a_state_directory_in_its_snapshot_store_before_it_provisions() {
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let state = store.join("snapshots").join(hex(b"first"));
    let Ok((Command::Restore(options), _)) = command(&[
        "restore",
        "first",
        "--store-dir",
        store.to_str().unwrap(),
        "--state-dir",
        state.to_str().unwrap(),
    ]) else {
        panic!("restore parses");
    };
    let error = restore(None, *options).unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::Unsupported), "{error:?}");
    assert!(!store.exists());
}

#[test]
fn a_restore_refuses_a_workload_that_is_not_a_valid_exec_request_before_it_provisions() {
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let state = root.path().join("state");
    let restore_of = [
        "restore",
        "first",
        "--store-dir",
        store.to_str().unwrap(),
        "--state-dir",
        state.to_str().unwrap(),
    ];
    for (workload, expected) in [
        (
            &["--entrypoint", "echo"][..],
            "managed exec entrypoint must be absolute",
        ),
        (
            &["--arg", "x"].repeat(64)[..],
            "managed exec requires 1 through 64 arguments",
        ),
    ] {
        let Ok((Command::Restore(options), _)) = command(&[&restore_of[..], workload].concat())
        else {
            panic!("restore parses");
        };
        let error = restore(None, *options).unwrap_err();
        assert_eq!(error.code(), Some(ErrorCode::Unsupported), "{error:?}");
        let message = error.message().unwrap_or_default();
        assert!(message.contains(expected), "{message}");
    }
    assert!(!store.exists());
    assert!(!state.exists());
}

/// Returns a store with the published snapshot `first`, and a VM directory with a view of it.
fn restored_view(root: &Path) -> (Store, VmDir) {
    fs::create_dir_all(root).unwrap();
    let store = Store::create(&root.join("store")).unwrap();
    let held = store.lock(&name("first"), Deadline::after(0.0)).unwrap();
    held.publish(staged(&held)).unwrap();
    let path = root.join("vm");
    crate::host::vm::create_private_dir(&path).unwrap();
    let vm = VmDir::open(&path).unwrap();
    let view = path.join(OPENVMM_DIR);
    crate::host::vm::create_private_dir(&view).unwrap();
    for name in SNAPSHOT_ARTIFACTS {
        fs::hard_link(held.dir.join(OPENVMM_DIR).join(name), view.join(name)).unwrap();
    }
    drop(held);
    (store, vm)
}

#[test]
fn removing_a_view_takes_only_the_snapshots_links_and_keeps_the_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let (store, vm) = restored_view(root.path());
    let held = store.lock(&name("first"), Deadline::after(0.0)).unwrap();
    held.remove_view(&vm).unwrap();
    assert!(!vm.path().join(OPENVMM_DIR).exists());
    held.remove_view(&vm).unwrap();
    held.open().unwrap();
    // A view that holds anything else keeps it, and one that is a link keeps its target.
    let (store, vm) = restored_view(&root.path().join("extra"));
    fs::write(vm.path().join(OPENVMM_DIR).join("other"), b"kept").unwrap();
    let held = store.lock(&name("first"), Deadline::after(0.0)).unwrap();
    let error = held.remove_view(&vm).unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::ForeignFiles), "{error:?}");
    assert_eq!(
        fs::read(vm.path().join(OPENVMM_DIR).join("other")).unwrap(),
        b"kept"
    );
    let (store, vm) = restored_view(&root.path().join("linked"));
    let view = vm.path().join(OPENVMM_DIR);
    let target = root.path().join("target");
    fs::rename(&view, &target).unwrap();
    link_directory(&view, &target);
    let held = store.lock(&name("first"), Deadline::after(0.0)).unwrap();
    let error = held.remove_view(&vm).unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::ForeignFiles), "{error:?}");
    assert_eq!(fs::read_dir(&target).unwrap().count(), 3);
    // A VM directory in the store, here the snapshot's own, whose `openvmm/` holds the snapshot's
    // files, is refused before anything goes.
    let own = VmDir::open(&held.dir).unwrap();
    let error = held.remove_view(&own).unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::Unsupported), "{error:?}");
    held.open().unwrap();
}
