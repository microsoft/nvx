//! Tests of the store's identities, records, lock, and launch bookkeeping. The launches of real
//! processes and the recovery of interrupted ones are tested in `tests/store.rs`.

use std::fs;
use std::sync::mpsc;
use std::thread;

use super::*;
use crate::ErrorKind;
use crate::contract;
use crate::host::process::{BootId, ProcessRef};
use crate::host::test_support::released;

/// A launch capability.
const CAPABILITY: [u8; CAPABILITY_LEN] = [7; CAPABILITY_LEN];

/// Returns a reference to process 42, started at 4242 during the boot whose bytes are all `boot`.
fn reference(boot: u8) -> ProcessRef {
    ProcessRef {
        pid: 42,
        start_time: 4242,
        boot: BootId::from_bytes([boot; 16]),
    }
}

/// Returns the identity `sha256:` and 64 copies of `digit`.
fn artifact(digit: char) -> ArtifactIdentity {
    ArtifactIdentity::sha256(&digit.to_string().repeat(64)).unwrap()
}

/// Returns a runtime identity under this crate's contracts.
fn runtime() -> RuntimeIdentity {
    RuntimeIdentity::new(artifact('a'), artifact('b'), artifact('c'))
}

/// Returns a store in a fresh temporary directory.
fn store() -> (tempfile::TempDir, SandboxStore) {
    let directory = tempfile::tempdir().unwrap();
    let store = SandboxStore::create(directory.path().join("sandbox")).unwrap();
    (directory, store)
}

/// Writes `value` as the file `name` of `store`.
fn write(store: &SandboxStore, name: &str, value: &serde_json::Value) {
    fs::write(store.path(name), serde_json::to_vec(value).unwrap()).unwrap();
}

#[test]
fn artifact_identities_are_printable_tokens() {
    assert_eq!(
        ArtifactIdentity::new("openvmm-1.2+build.7")
            .unwrap()
            .as_str(),
        "openvmm-1.2+build.7"
    );
    for invalid in [
        String::new(),
        "with space".to_owned(),
        "tab\there".to_owned(),
        "caf\u{e9}".to_owned(),
        "x".repeat(MAX_IDENTITY_LEN + 1),
    ] {
        let error = ArtifactIdentity::new(invalid.clone()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{invalid:?}");
    }
    assert!(ArtifactIdentity::new("x".repeat(MAX_IDENTITY_LEN)).is_ok());

    let digest = "AB".repeat(32);
    assert_eq!(
        ArtifactIdentity::sha256(&digest).unwrap().to_string(),
        format!("sha256:{}", "ab".repeat(32))
    );
    for invalid in ["ab".repeat(31), "g".repeat(64), "ab".repeat(33)] {
        assert_eq!(
            ArtifactIdentity::sha256(&invalid).unwrap_err().kind(),
            ErrorKind::Unsupported
        );
    }
    // The serde form checks the token too.
    assert!(serde_json::from_value::<ArtifactIdentity>(serde_json::json!("a b")).is_err());
}

#[test]
fn runtime_identities_bind_this_crates_contracts() {
    let identity = runtime();
    assert_eq!(identity.boot_contract, contract::BOOT_CONTRACT_VERSION);
    assert_eq!(
        identity.control_contract,
        contract::CONTROL_CONTRACT_REVISION
    );
    assert_eq!(
        identity.control_session,
        contract::CONTROL_SESSION_PROTOCOL_VERSION
    );
    identity.check().unwrap();
    assert!(identity.differences(&runtime()).is_empty());

    let mut other = runtime();
    other.kernel = artifact('d');
    other.control_session += 1;
    assert_eq!(
        identity.differences(&other),
        [IdentityPart::Kernel, IdentityPart::ControlSession]
    );
    other = RuntimeIdentity::new(artifact('e'), artifact('b'), artifact('f'));
    other.boot_contract += 1;
    other.control_contract = "another-revision".to_owned();
    assert_eq!(
        identity.differences(&other),
        [
            IdentityPart::OpenVmm,
            IdentityPart::Initramfs,
            IdentityPart::BootContract,
            IdentityPart::ControlContract
        ]
    );
    let names: Vec<_> = IdentityPart::ALL.iter().map(ToString::to_string).collect();
    assert_eq!(
        names,
        [
            "OpenVMM",
            "kernel",
            "initramfs",
            "boot contract",
            "control contract",
            "control session"
        ]
    );
    for invalid in ["", "two words"] {
        other.control_contract = invalid.to_owned();
        assert_eq!(other.check().unwrap_err().kind(), ErrorKind::Unsupported);
    }
}

#[test]
fn records_keep_their_json_form() {
    let journal = LaunchJournal::prepared(
        LaunchId::try_from("0123456789abcdef0123456789abcdef".to_owned()).unwrap(),
        "/state/sandbox/control.sock".to_owned(),
        runtime(),
        LogClaim {
            identity: FileIdentity {
                device: 1,
                index: 2,
            },
        },
    );
    let runtime_json = serde_json::json!({
        "openvmm": format!("sha256:{}", "a".repeat(64)),
        "kernel": format!("sha256:{}", "b".repeat(64)),
        "initramfs": format!("sha256:{}", "c".repeat(64)),
        "bootContract": contract::BOOT_CONTRACT_VERSION,
        "controlContract": contract::CONTROL_CONTRACT_REVISION,
        "controlSession": contract::CONTROL_SESSION_PROTOCOL_VERSION,
    });
    let prepared = serde_json::json!({
        "format": 4,
        "launch": "0123456789abcdef0123456789abcdef",
        "endpoint": "/state/sandbox/control.sock",
        "runtime": runtime_json,
        "log": { "identity": { "device": 1, "index": 2 } },
    });
    assert_eq!(serde_json::to_value(&journal).unwrap(), prepared);
    assert_eq!(journal.state(), LaunchState::Prepared);

    let process = reference(0xab);
    let spawned = journal.spawned(process.clone());
    assert_eq!(spawned.state(), LaunchState::Spawned);
    let process_json = serde_json::json!({
        "pid": 42,
        "startTime": 4242,
        "boot": "ab".repeat(16),
    });
    let mut expected = prepared.clone();
    expected["process"] = process_json.clone();
    assert_eq!(serde_json::to_value(&spawned).unwrap(), expected);

    let record = RuntimeRecord::committed(&spawned, process);
    assert_eq!(
        serde_json::to_value(&record).unwrap(),
        serde_json::json!({
            "format": 4,
            "launch": "0123456789abcdef0123456789abcdef",
            "process": process_json,
            "endpoint": "/state/sandbox/control.sock",
            "runtime": runtime_json,
        })
    );
    assert_eq!(record.check_identity(&runtime()), IdentityCheck::Matches);
    let mut upgraded = runtime();
    upgraded.openvmm = artifact('9');
    assert_eq!(
        record.check_identity(&upgraded),
        IdentityCheck::Mismatched(vec![IdentityPart::OpenVmm])
    );
}

#[test]
fn records_of_other_formats_fail_clearly() {
    let (_directory, store) = store();
    let unsupported = |value: serde_json::Value, format: &str| {
        write(&store, RUNTIME_NAME, &value);
        let error = store.runtime().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{error}");
        assert!(
            error
                .message()
                .contains(&format!("has the unsupported format {format};")),
            "{error}"
        );
        assert!(error.message().contains("migrates no other"), "{error}");
    };
    // The runtime records of the CLI's state directories and of `instance`.
    unsupported(
        serde_json::json!({
            "format": 1, "pid": 7, "start_time": 8, "control_endpoint": "/state/control.sock",
        }),
        "1",
    );
    unsupported(
        serde_json::json!({ "format": 3, "pid": 7, "startTime": 8, "endpoint": "e" }),
        "3",
    );
    unsupported(serde_json::json!({ "format": 5 }), "5");
    unsupported(serde_json::json!({ "format": "4" }), "\"4\"");

    let malformed = |text: &[u8], detail: &str| {
        fs::write(store.path(JOURNAL_NAME), text).unwrap();
        let error = store.journal().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{error}");
        assert!(error.message().contains("is malformed"), "{error}");
        assert!(error.message().contains(detail), "{error}");
    };
    malformed(b"{ truncated", "");
    malformed(b"[4]", "it has no format");
    malformed(b"{}", "it has no format");
    malformed(br#"{ "format": 4 }"#, "");
}

#[test]
fn records_with_invalid_fields_are_malformed() {
    let (_directory, store) = store();
    let valid = serde_json::json!({
        "format": 4,
        "launch": "0123456789abcdef0123456789abcdef",
        "process": { "pid": 42, "startTime": 4242, "boot": "ab".repeat(16) },
        "endpoint": "/state/sandbox/control.sock",
        "runtime": serde_json::to_value(runtime()).unwrap(),
    });
    write(&store, RUNTIME_NAME, &valid);
    let record = store.runtime().unwrap().unwrap();
    assert_eq!(record.process(), &reference(0xab));
    assert_eq!(record.endpoint(), "/state/sandbox/control.sock");
    assert_eq!(record.runtime(), &runtime());

    let invalid = |pointer: &str, value: serde_json::Value, detail: &str| {
        let mut record = valid.clone();
        *record.pointer_mut(pointer).unwrap() = value;
        write(&store, RUNTIME_NAME, &record);
        let error = store.runtime().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{pointer}: {error}");
        assert!(
            error.message().contains("is malformed"),
            "{pointer}: {error}"
        );
        assert!(error.message().contains(detail), "{pointer}: {error}");
    };
    invalid("/process/pid", 0.into(), "its process ID is 0");
    invalid("/process/boot", "ab".into(), "");
    invalid("/endpoint", "".into(), "its control endpoint");
    invalid("/runtime/controlContract", "a b".into(), "control contract");
    invalid("/launch", "0123".into(), "");
    invalid("/runtime/kernel", "".into(), "");
    invalid("/runtime/bootContract", (-1).into(), "");
    // Unknown fields are refused rather than ignored.
    let mut extra = valid.clone();
    extra["capability"] = "secret".into();
    write(&store, RUNTIME_NAME, &extra);
    assert_eq!(store.runtime().unwrap_err().kind(), ErrorKind::Io);
}

#[test]
fn the_lock_excludes_other_holders() {
    let (_directory, store) = store();
    let held = store.lock(None).unwrap();
    let started = Instant::now();
    let error = store
        .lock(Some(Instant::now() + Duration::from_millis(100)))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TimedOut, "{error}");
    assert!(started.elapsed() >= Duration::from_millis(100));
    // A deadline that has passed still tries once.
    assert!(store.lock(Some(Instant::now())).is_err());
    drop(held);
    assert!(released(|| match store.lock(Some(Instant::now())) {
        Ok(_) => true,
        Err(error) if error.kind() == ErrorKind::TimedOut => false,
        Err(error) => panic!("{error}"),
    }));

    // A waiter without a deadline takes the lock once its holder releases it.
    let held = store.lock(None).unwrap();
    let (sender, receiver) = mpsc::channel();
    let waiter = thread::spawn({
        let store = store.clone();
        move || {
            let locked = store.lock(None).map(drop);
            sender.send(()).unwrap();
            locked
        }
    });
    assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err());
    drop(held);
    waiter.join().unwrap().unwrap();
}

#[test]
fn a_lock_file_replaced_while_waiting_locks_nothing() {
    let (_directory, store) = store();
    let held = store.lock(None).unwrap();
    let waiter = thread::spawn({
        let store = store.clone();
        move || {
            let locked = store.lock(None).unwrap();
            // The waiter holds the lock of the file that the path names now.
            let current = platform::file_id(&locked.path(LOCK_NAME)).unwrap();
            let held = platform::opened_file_id(&locked.lock).unwrap();
            assert_eq!(current, held);
        }
    });
    thread::sleep(Duration::from_millis(100));
    // The lock file goes while the waiter waits for it.
    fs::remove_file(store.path(LOCK_NAME)).unwrap();
    drop(held);
    waiter.join().unwrap();
}

#[cfg(unix)]
#[test]
fn a_lock_file_that_is_not_a_regular_file_fails_at_once() {
    use std::os::unix::fs::OpenOptionsExt;

    use nix::sys::stat::Mode;

    let (_directory, store) = store();
    let path = store.path(LOCK_NAME);
    nix::unistd::mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
    let refused = |error: Error| {
        assert_eq!(error.message(), format!("cannot lock {}", path.display()));
        let cause = std::error::Error::source(&error).map(ToString::to_string);
        assert_eq!(cause.as_deref(), Some("is not a regular file"), "{error}");
    };
    // An open of a FIFO for writing that waits would wait for a reader, so the lock is taken on a
    // thread that the test can leave behind.
    let (sender, receiver) = mpsc::channel();
    thread::spawn({
        let store = store.clone();
        move || sender.send(store.lock(None).map(drop)).unwrap()
    });
    let locked = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("the lock waits for a reader of the FIFO");
    refused(locked.unwrap_err());
    // A FIFO that has a reader opens, and is refused all the same.
    let _reader = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits())
        .open(&path)
        .unwrap();
    refused(store.lock(Some(Instant::now())).unwrap_err());
}

#[test]
fn a_lock_file_that_is_a_link_fails_at_once() {
    let (_directory, store) = store();
    let path = store.path(LOCK_NAME);
    // A lock taken through a link to a file that a launch replaces, such as the capability, would
    // hold the replaced file, and the next transition would lock its replacement.
    let target = store.capability_path();
    fs::write(&target, CAPABILITY).unwrap();
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&target, &path);
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_file(&target, &path);
    if linked.is_err() {
        // Windows lets only Developer Mode or a privilege create a symbolic link.
        eprintln!("skipped: this host does not let the test create a symbolic link");
        return;
    }
    let error = store.lock(None).unwrap_err();
    assert_eq!(error.message(), format!("cannot lock {}", path.display()));
    let cause = std::error::Error::source(&error).map(ToString::to_string);
    assert_eq!(cause.as_deref(), Some("is not a regular file"), "{error}");
    // The link and the file that it names stay as they are.
    assert!(
        fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&target).unwrap(), CAPABILITY);
}

// Other hosts open no handle of a sandbox's directory, through which a clone's files go.
#[cfg(any(target_os = "linux", windows))]
#[test]
fn the_files_that_a_clone_kept_of_its_launch_go_and_nothing_else() {
    let (_directory, store) = store();
    let locked = store.lock(None).unwrap();
    for path in [
        // The file of the scratch device that an earlier version left for a clone in RAM.
        store.dir().join(CLONE_SCRATCH_NAME),
        store.clone_activation_path(),
        store.clone_progress_path(),
        store.clone_failure_path(),
        store.restore_ready_path(),
        store.log_path(),
        store.binding_path(),
    ] {
        fs::write(path, b"left").unwrap();
    }
    let directory = locked.open_dir().unwrap();
    locked.remove_clone_files(&directory).unwrap();
    let mut left: Vec<String> = fs::read_dir(store.dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    // The binding goes later, once the release or the removal has removed the clone's files.
    let mut expected = [CLONE_BINDING_NAME, LOCK_NAME, LOG_NAME];
    expected.sort_unstable();
    assert_eq!(left, expected);
    // Nothing to remove is no failure.
    locked.remove_clone_files(&directory).unwrap();
}

#[cfg(unix)]
#[test]
fn records_and_capabilities_that_are_not_regular_files_fail_at_once() {
    use std::os::unix::fs::OpenOptionsExt;

    use nix::sys::stat::Mode;

    let (_directory, store) = store();
    for (name, description) in [
        (RUNTIME_NAME, "sandbox runtime record"),
        (JOURNAL_NAME, "sandbox launch journal"),
        (CAPABILITY_NAME, "sandbox control capability"),
    ] {
        let path = store.path(name);
        nix::unistd::mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        // An open of a FIFO for reading that waits would wait for a writer, and a read of one would
        // wait until its writer closes it, so the file is read on a thread that the test can leave
        // behind.
        let refused = || {
            let (sender, receiver) = mpsc::channel();
            thread::spawn({
                let store = store.clone();
                move || {
                    let read = match name {
                        RUNTIME_NAME => store.runtime().map(drop),
                        JOURNAL_NAME => store.journal().map(drop),
                        _ => store.read_capability().map(drop),
                    };
                    sender.send(read).unwrap();
                }
            });
            let error = receiver
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|_| panic!("the read of the {description} waits on the FIFO"))
                .unwrap_err();
            assert_eq!(
                error.message(),
                format!("cannot read the {description} {}", path.display())
            );
            let cause = std::error::Error::source(&error).map(ToString::to_string);
            assert_eq!(cause.as_deref(), Some("is not a regular file"), "{error}");
        };
        refused();
        // A FIFO that has a writer opens at once, and is refused all the same, before a read waits
        // for the writer to close it.
        let reader = fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits())
            .open(&path)
            .unwrap();
        let writer = fs::OpenOptions::new()
            .write(true)
            .custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits())
            .open(&path)
            .unwrap();
        drop(reader);
        refused();
        drop(writer);
        fs::remove_file(&path).unwrap();
    }
}

#[test]
fn failures_to_read_the_records_and_the_capability_name_them() {
    let (_directory, store) = store();
    let read = |name: &str| match name {
        RUNTIME_NAME => store.runtime().map(drop),
        JOURNAL_NAME => store.journal().map(drop),
        _ => store.read_capability().map(drop),
    };
    let files = [
        (RUNTIME_NAME, "sandbox runtime record"),
        (JOURNAL_NAME, "sandbox launch journal"),
        (CAPABILITY_NAME, "sandbox control capability"),
    ];
    // A missing record is none, but a missing capability fails, as its launch needs it.
    assert!(store.runtime().unwrap().is_none());
    assert!(store.journal().unwrap().is_none());
    let path = store.path(CAPABILITY_NAME);
    let error = store.read_capability().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert_eq!(
        error.message(),
        format!(
            "cannot read the sandbox control capability {}",
            path.display()
        )
    );
    let cause = std::error::Error::source(&error)
        .and_then(|source| source.downcast_ref::<io::Error>())
        .map(io::Error::kind);
    assert_eq!(cause, Some(io::ErrorKind::NotFound), "{error}");
    // A capability of another length, or an all-zero one, is invalid.
    for invalid in [&CAPABILITY[1..], &[0; CAPABILITY_LEN][..]] {
        fs::write(&path, invalid).unwrap();
        let error = store.read_capability().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io);
        assert_eq!(
            error.message(),
            format!(
                "the sandbox control capability {} is invalid",
                path.display()
            )
        );
    }
    fs::remove_file(&path).unwrap();
    // A record that does not parse is malformed.
    for (name, description) in &files[..2] {
        let path = store.path(name);
        fs::write(&path, b"{").unwrap();
        let error = read(name).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io);
        assert_eq!(
            error.message(),
            format!("the {description} {} is malformed", path.display())
        );
        fs::remove_file(&path).unwrap();
    }
    // A directory in the place of a record or of the capability cannot be read as one: Unix opens
    // it, and refuses it as no regular file, and Windows refuses to open it.
    for (name, description) in files {
        let path = store.path(name);
        fs::create_dir(&path).unwrap();
        let error = read(name).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io);
        assert_eq!(
            error.message(),
            format!("cannot read the {description} {}", path.display())
        );
        fs::remove_dir(&path).unwrap();
    }
}

#[cfg(target_os = "linux")]
#[test]
fn the_store_is_private_to_the_current_user() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let (directory, store) = store();
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o7777;
    let set_mode = |path: &Path, mode: u32| {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    };
    // A directory that others could access is refused, and left as it is, rather than restricted:
    // another user could hold its files open, as this journal that anyone may write to, which a
    // change of the directory's mode would not take back.
    let shared = directory.path().join("shared");
    fs::create_dir(&shared).unwrap();
    let journal = shared.join(JOURNAL_NAME);
    fs::write(&journal, b"{}").unwrap();
    set_mode(&journal, 0o666);
    let held = fs::OpenOptions::new().write(true).open(&journal).unwrap();
    for shared_mode in [0o755, 0o777] {
        set_mode(&shared, shared_mode);
        for error in [
            SandboxStore::create(&shared).unwrap_err(),
            SandboxStore::open(&shared).unwrap_err(),
        ] {
            let expected = format!("is accessible to other users (mode {shared_mode:03o})");
            assert!(error.message().ends_with(&expected), "{error}");
        }
        assert_eq!(mode(&shared), shared_mode);
    }
    drop(held);
    // A link to a store is no store.
    let link = directory.path().join("link");
    symlink(store.dir(), &link).unwrap();
    for error in [
        SandboxStore::create(&link).unwrap_err(),
        SandboxStore::open(&link).unwrap_err(),
    ] {
        assert!(
            error.message().ends_with("is not a plain directory"),
            "{error}"
        );
    }
    // The store's own directory and files are the user's alone.
    assert_eq!(mode(store.dir()), 0o700);
    assert_eq!(SandboxStore::open(store.dir()).unwrap(), store);
    assert_eq!(SandboxStore::create(store.dir()).unwrap(), store);
    let mut locked = store.lock(None).unwrap();
    let launch = locked
        .begin_launch(&LaunchId::generate().unwrap(), &runtime(), &CAPABILITY)
        .unwrap();
    drop(launch);
    for name in [LOCK_NAME, JOURNAL_NAME, CAPABILITY_NAME, LOG_NAME] {
        assert_eq!(mode(&store.path(name)), 0o600, "{name}");
    }
    // A new store takes the setgid bit of its parent, and drops it for the mode 0700 that OpenVMM
    // requires of the directory of its control socket; an existing directory with another mode is
    // refused.
    let setgid = directory.path().join("setgid");
    fs::create_dir(&setgid).unwrap();
    set_mode(&setgid, 0o2700);
    assert_eq!(mode(&setgid), 0o2700);
    let plain = setgid.join("plain");
    fs::create_dir(&plain).unwrap();
    assert_eq!(mode(&plain) & 0o2000, 0o2000);
    let nested = SandboxStore::create(setgid.join("sandbox")).unwrap();
    assert_eq!(mode(nested.dir()), 0o700);
    for error in [
        SandboxStore::create(&setgid).unwrap_err(),
        SandboxStore::open(&setgid).unwrap_err(),
    ] {
        assert!(
            error.message().ends_with(
                "has mode 2700, not the mode 700 that the directory of a control socket must have"
            ),
            "{error}"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn stores_belong_to_the_current_user_with_mode_700() {
    use nix::sys::stat::Mode;
    use nix::unistd::Uid;

    // The check takes the owner and the current user as user IDs, and the permission bits of the
    // directory's mode, without the bits of its type, as the store reads them from its metadata.
    let store_dir_problem = |uid, mode, euid| {
        platform::store_dir_problem(
            Uid::from_raw(uid),
            Mode::from_bits_truncate(mode),
            Uid::from_raw(euid),
        )
    };
    // Another user's directory is refused whatever its mode, since its owner may change it.
    assert_eq!(
        store_dir_problem(1001, 0o40700, 1000),
        Some("belongs to user 1001, not to the current user 1000".to_owned())
    );
    assert_eq!(store_dir_problem(1000, 0o40700, 1000), None);
    for mode in [0o40750, 0o40705, 0o40770] {
        let problem = store_dir_problem(1000, mode, 1000).unwrap();
        assert!(
            problem.starts_with("is accessible to other users"),
            "{problem}"
        );
    }
    for (mode, shown) in [(0o42700, "2700"), (0o41700, "1700"), (0o40500, "500")] {
        let problem = store_dir_problem(1000, mode, 1000).unwrap();
        assert!(
            problem.starts_with(&format!("has mode {shown}, ")),
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
    let error = SandboxStore::open(directory.path()).unwrap_err();
    assert!(error.message().ends_with("allows other users"), "{error}");
    let shared = directory.path().join("shared");
    fs::create_dir(&shared).unwrap();
    let error = SandboxStore::create(&shared).unwrap_err();
    assert!(error.message().ends_with("allows other users"), "{error}");
    // Nor does one that allows its owner alone, but passes nothing on to the files created in it,
    // which would take their creator's default list.
    platform::set_access(&shared, "D:P(A;;FA;;;OW)").unwrap();
    for error in [
        SandboxStore::create(&shared).unwrap_err(),
        SandboxStore::open(&shared).unwrap_err(),
    ] {
        assert!(
            error
                .message()
                .ends_with("passes no access control entries on to the files created in it"),
            "{error}"
        );
    }
    // A store created there allows the current user alone, and so do its files.
    let store = SandboxStore::create(directory.path().join("sandbox")).unwrap();
    assert_eq!(SandboxStore::open(store.dir()).unwrap(), store);
    assert_eq!(SandboxStore::create(store.dir()).unwrap(), store);
    let mut locked = store.lock(None).unwrap();
    let launch = locked
        .begin_launch(&LaunchId::generate().unwrap(), &runtime(), &CAPABILITY)
        .unwrap();
    drop(launch);
    let problem = |name: &str| {
        let file = fs::File::open(store.path(name)).unwrap();
        platform::private_file_problem(&file).unwrap()
    };
    for name in [LOCK_NAME, JOURNAL_NAME, CAPABILITY_NAME, LOG_NAME] {
        assert_eq!(problem(name), None, "{name}");
    }
    // A record or a capability whose own list allows others could have been written by them,
    // whatever the directory allows, and is refused.
    for name in [JOURNAL_NAME, CAPABILITY_NAME] {
        platform::set_access(&store.path(name), "D:P(A;;FA;;;WD)").unwrap();
        assert_eq!(
            problem(name).as_deref(),
            Some("allows other users"),
            "{name}"
        );
    }
    let refused = |description: &str, name: &str| {
        format!(
            "the {description} {} allows other users",
            store.path(name).display()
        )
    };
    let error = store.journal().unwrap_err();
    assert_eq!(
        error.message(),
        refused("sandbox launch journal", JOURNAL_NAME)
    );
    let error = locked.recover().unwrap_err();
    assert_eq!(
        error.message(),
        refused("sandbox launch journal", JOURNAL_NAME)
    );
    let error = store.read_capability().unwrap_err();
    assert_eq!(
        error.message(),
        refused("sandbox control capability", CAPABILITY_NAME)
    );
    // So is a lock file whose own list allows others, who could hold its lock.
    drop(locked);
    platform::set_access(&store.path(LOCK_NAME), "D:P(A;;FA;;;WD)").unwrap();
    let error = store.lock(None).unwrap_err();
    assert_eq!(error.message(), refused("sandbox lock", LOCK_NAME));
}

#[test]
fn a_caller_that_keeps_the_console_keeps_it_across_launches() {
    assert!(!CallerFiles::new("config.json").unwrap().keeps_console());
    let files = CallerFiles::new("config.json").unwrap().keeping_console();
    assert!(files.keeps_console());
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path().join("sandbox");
    let mut locked = SandboxStore::lock_for_provision(&dir, &files, None).unwrap();
    fs::write(dir.join("config.json"), b"{}").unwrap();
    let (console, outcome) = (locked.console_path(), locked.outcome_path());
    fs::write(&console, b"earlier launches\n").unwrap();
    fs::write(&outcome, b"{}").unwrap();
    let launch = locked
        .begin_launch(&LaunchId::generate().unwrap(), &runtime(), &CAPABILITY)
        .unwrap();
    // The launch leaves the console of the earlier launches for the caller to append to, and
    // removes the previous launch's outcome report, as for any caller.
    assert_eq!(fs::read(&console).unwrap(), b"earlier launches\n");
    assert!(!outcome.exists());
    match launch.abandon().unwrap() {
        Recovery::Exited => {}
        // A process that a concurrent test spawned may still hold the log that the launch closed,
        // until it executes its program, and the next recovery then ends the launch.
        Recovery::Indeterminate(_) => {
            assert!(released(|| locked.recover().unwrap() == Recovery::Exited));
        }
        other => panic!("{other:?}"),
    }
    // The removal removes the console log with the store's other files.
    assert!(locked.remove().unwrap().is_none());
    assert!(!dir.exists());
}

#[test]
fn stores_open_only_existing_directories() {
    let (directory, store) = store();
    assert_eq!(SandboxStore::open(store.dir()).unwrap(), store);
    let missing = SandboxStore::open(directory.path().join("missing")).unwrap_err();
    assert_eq!(missing.kind(), ErrorKind::NotFound);
    let file = directory.path().join("file");
    fs::write(&file, b"").unwrap();
    assert!(
        SandboxStore::open(&file)
            .unwrap_err()
            .message()
            .contains("is not a plain directory")
    );
    // A relative directory names the same store from any later working directory.
    let current = absolute(PathBuf::from(".")).unwrap();
    assert!(current.is_absolute());
    assert_eq!(
        platform::file_identity(&current).unwrap(),
        platform::file_identity(&std::env::current_dir().unwrap()).unwrap()
    );
}

#[test]
fn a_launch_writes_its_marker_before_its_vmm_runs() {
    let (_directory, store) = store();
    for name in [OUTCOME_NAME, CONSOLE_NAME, LOG_NAME] {
        fs::write(store.path(name), b"previous launch").unwrap();
    }
    let previous_log = platform::file_identity(&store.log_path()).unwrap();
    // Held open, the previous log keeps its inode, which the fresh log cannot then reuse.
    let previous = fs::File::open(store.log_path()).unwrap();
    let mut locked = store.lock(None).unwrap();
    let mut launch = locked
        .begin_launch(&LaunchId::generate().unwrap(), &runtime(), &CAPABILITY)
        .unwrap();
    let journal = store.journal().unwrap().unwrap();
    assert_eq!(&journal, launch.journal());
    assert_eq!(journal.state(), LaunchState::Prepared);
    assert_eq!(journal.runtime(), &runtime());
    assert_eq!(launch.endpoint(), journal.endpoint());
    if cfg!(windows) {
        assert!(journal.endpoint().starts_with("//./pipe/openvmm-microvm-"));
    } else {
        assert_eq!(Path::new(journal.endpoint()), store.socket_path());
    }
    assert_eq!(store.read_capability().unwrap(), CAPABILITY);
    // The previous launch's outcome and console are gone, and its log is replaced.
    assert!(!store.outcome_path().exists());
    assert!(!store.console_path().exists());
    assert_eq!(fs::read(store.log_path()).unwrap(), b"");
    let identity = platform::file_identity(&store.log_path()).unwrap();
    assert_ne!(identity, previous_log);
    let claim = journal.log();
    assert_eq!((claim.identity.device, claim.identity.index), identity);

    // The claim lasts until every holder of the log closes it.
    let log = launch.take_log().unwrap();
    assert!(launch.take_log().is_err());
    assert!(platform::log_claim_held(&store.log_path(), identity).unwrap());
    let copy = log.try_clone().unwrap();
    drop(log);
    assert!(platform::log_claim_held(&store.log_path(), identity).unwrap());
    drop(copy);
    let free = || !platform::log_claim_held(&store.log_path(), identity).unwrap();
    assert!(released(free));
    // A log that is not the claimed file proves nothing.
    assert!(platform::log_claim_held(&store.log_path(), previous_log).is_err());

    // A second launch waits until the first is recovered.
    drop(launch);
    let error = locked
        .begin_launch(&LaunchId::generate().unwrap(), &runtime(), &CAPABILITY)
        .unwrap_err();
    assert!(
        error.message().contains("must be recovered first"),
        "{error}"
    );
    assert_eq!(locked.recover().unwrap(), Recovery::Exited);
    assert!(store.journal().unwrap().is_none());
    assert!(store.read_capability().is_err());
    // The log outlives the launch.
    assert!(store.log_path().exists());
    assert_eq!(locked.recover().unwrap(), Recovery::Clean);
    drop(previous);
}

#[test]
fn launches_check_their_inputs() {
    let (_directory, store) = store();
    let mut locked = store.lock(None).unwrap();
    let error = locked
        .begin_launch(
            &LaunchId::generate().unwrap(),
            &runtime(),
            &[0; CAPABILITY_LEN],
        )
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    let mut invalid = runtime();
    invalid.control_contract = String::new();
    assert_eq!(
        locked
            .begin_launch(&LaunchId::generate().unwrap(), &invalid, &CAPABILITY)
            .unwrap_err()
            .kind(),
        ErrorKind::Unsupported
    );
    // Nothing was written.
    assert!(store.journal().unwrap().is_none());
    assert!(!store.capability_path().exists());
    assert!(!store.log_path().exists());
}

#[test]
fn an_abandoned_launch_that_spawned_nothing_has_exited() {
    let (_directory, store) = store();
    let mut locked = store.lock(None).unwrap();
    let launch = locked
        .begin_launch(&LaunchId::generate().unwrap(), &runtime(), &CAPABILITY)
        .unwrap();
    // A launch commits only once it records its VMM.
    let error = launch.commit().unwrap_err();
    assert!(error.message().contains("before it records"), "{error}");
    let error = locked
        .begin_launch(&LaunchId::generate().unwrap(), &runtime(), &CAPABILITY)
        .unwrap_err();
    assert!(
        error.message().contains("must be recovered first"),
        "{error}"
    );
    // A process that a concurrent test spawned may still hold the log that the launch closed,
    // until it executes its program, and a later recovery then ends the launch.
    assert!(released(|| locked.recover().unwrap() == Recovery::Exited));
    let launch = locked
        .begin_launch(&LaunchId::generate().unwrap(), &runtime(), &CAPABILITY)
        .unwrap();
    match launch.abandon().unwrap() {
        Recovery::Exited => {}
        Recovery::Indeterminate(_) => {
            assert!(released(|| locked.recover().unwrap() == Recovery::Exited));
        }
        other => panic!("{other:?}"),
    }
    assert!(store.journal().unwrap().is_none());
    assert!(!store.capability_path().exists());
}

#[test]
fn a_recorded_vmm_that_cannot_be_held_has_exited_once_its_log_is_released() {
    let (_directory, store) = store();
    let mut locked = store.lock(None).unwrap();
    let mut launch = locked
        .begin_launch(&LaunchId::generate().unwrap(), &runtime(), &CAPABILITY)
        .unwrap();
    let log = launch.take_log().unwrap();
    let reference = ProcessRef {
        boot: BootId::current().unwrap(),
        ..reference(0)
    };
    let journal = launch.journal().spawned(reference.clone());
    drop(launch);
    // No process matches the reference, or the process that has its ID, such as another user's,
    // cannot be examined.
    /// Holds the process that a reference names, as `Process::open` does.
    type Open = fn(&ProcessRef) -> io::Result<Option<Process>>;
    let opens: [Open; 2] = [
        |_| Ok(None),
        |_| Err(io::Error::new(io::ErrorKind::PermissionDenied, "hidden")),
    ];
    // While a process of the launch holds its log, the launch may still run.
    for open in opens {
        let Recovery::Indeterminate(reason) = locked.end_identified(&journal, &reference, open)
        else {
            panic!("recovery ended a launch whose log is still held");
        };
        assert!(reason.contains("still holds its log"), "{reason}");
    }
    // Once nothing holds the log, the launch has exited. A process that a concurrent test spawned
    // may still hold it, until it executes its program.
    drop(log);
    for open in opens {
        let exited = || locked.end_identified(&journal, &reference, open) == Recovery::Exited;
        assert!(released(exited));
    }
}

#[test]
fn a_journal_that_its_runtime_record_supersedes_is_removed() {
    let (_directory, store) = store();
    let mut locked = store.lock(None).unwrap();
    let launch = locked
        .begin_launch(&LaunchId::generate().unwrap(), &runtime(), &CAPABILITY)
        .unwrap();
    let journal = launch.journal().spawned(reference(1));
    drop(launch);
    // The launch wrote its runtime record and crashed before it removed its journal.
    let committed = RuntimeRecord::committed(&journal, journal.process().unwrap().clone());
    record::write(&store.runtime_file(), &committed).unwrap();
    record::write(&store.journal_file(), &journal).unwrap();
    assert_eq!(locked.recover().unwrap(), Recovery::Clean);
    assert!(store.journal().unwrap().is_none());
    assert_eq!(store.runtime().unwrap(), Some(committed));
    // A runtime record of another launch makes the state inconsistent.
    let other = LaunchJournal::prepared(
        LaunchId::generate().unwrap(),
        journal.endpoint().to_owned(),
        runtime(),
        *journal.log(),
    );
    record::write(&store.journal_file(), &other).unwrap();
    let error = locked.recover().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert!(error.message().contains("names launch"), "{error}");
    // A runtime record keeps a launch from beginning until it is cleared.
    fs::remove_file(store.path(JOURNAL_NAME)).unwrap();
    let error = locked
        .begin_launch(&LaunchId::generate().unwrap(), &runtime(), &CAPABILITY)
        .unwrap_err();
    assert!(error.message().contains("already running"), "{error}");
    locked.clear_runtime().unwrap();
    assert!(store.runtime().unwrap().is_none());
    assert!(!store.capability_path().exists());
    drop(
        locked
            .begin_launch(&LaunchId::generate().unwrap(), &runtime(), &CAPABILITY)
            .unwrap(),
    );
}

#[test]
fn report_staging_files_go_once_nothing_of_their_launch_runs() {
    let (_directory, store) = store();
    // The file in which a VMM that was killed staged its outcome report, and other names, which
    // are not the store's.
    let staging = store.path(".tmpAbC123");
    let others = [".tmpAbC1234", ".tmpAb-123", ".tmp"];
    for name in others {
        fs::write(store.path(name), b"{").unwrap();
    }
    fs::write(&staging, b"{").unwrap();
    let mut locked = store.lock(None).unwrap();
    // A launch begins without the staging file of an earlier one.
    let mut launch = locked
        .begin_launch(&LaunchId::generate().unwrap(), &runtime(), &CAPABILITY)
        .unwrap();
    assert!(!staging.exists());
    // While a process of the launch may still hold its log, as its VMM does, the file stays.
    let log = launch.take_log().unwrap();
    drop(launch);
    fs::write(&staging, b"{").unwrap();
    let recovery = locked.recover().unwrap();
    assert!(
        matches!(recovery, Recovery::Indeterminate(_)),
        "{recovery:?}"
    );
    assert!(staging.exists());
    // Once nothing holds the log, recovery ends the launch and removes the file.
    drop(log);
    assert!(released(|| locked.recover().unwrap() == Recovery::Exited));
    assert!(!staging.exists());
    // An abandoned launch removes it, and so does clearing the runtime of a VM that exited.
    let launch = locked
        .begin_launch(&LaunchId::generate().unwrap(), &runtime(), &CAPABILITY)
        .unwrap();
    fs::write(&staging, b"{").unwrap();
    match launch.abandon().unwrap() {
        Recovery::Exited => {}
        // A process that a concurrent test spawned may still hold the log that the launch closed,
        // until it executes its program, and the next recovery then ends the launch.
        Recovery::Indeterminate(_) => {
            assert!(staging.exists());
            assert!(released(|| locked.recover().unwrap() == Recovery::Exited));
        }
        other => panic!("{other:?}"),
    }
    assert!(!staging.exists());
    fs::write(&staging, b"{").unwrap();
    locked.clear_runtime().unwrap();
    assert!(!staging.exists());
    for name in others {
        assert!(store.path(name).exists(), "{name}");
    }
}

#[test]
fn a_store_whose_directory_went_reports_it_missing_as_it_clears_a_runtime() {
    let (_directory, store) = store();
    let mut locked = store.lock(None).unwrap();
    // Something other than a removal deletes the directory while the lock is held.
    fs::remove_dir_all(store.dir()).unwrap();
    let error = locked.clear_runtime().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
}

#[test]
fn launch_identities_are_fresh_hexadecimal() {
    let first = LaunchId::generate().unwrap();
    assert_eq!(first.as_str().len(), 32);
    assert_ne!(first, LaunchId::generate().unwrap());
    assert!(LaunchId::try_from(first.to_string()).is_ok());
    for invalid in [
        String::new(),
        "0123".to_owned(),
        "A".repeat(32),
        "g".repeat(32),
    ] {
        assert!(LaunchId::try_from(invalid.clone()).is_err(), "{invalid}");
    }
}

#[test]
fn an_endpoint_server_counts_only_while_the_endpoint_names_it_once_held() {
    /// Holds the endpoint server whose IDs `lookups` returns in turn, as a value that names its
    /// ID, or `None` if `hold_exits`.
    fn hold(
        lookups: Vec<io::Result<Option<u32>>>,
        hold_exits: bool,
    ) -> std::result::Result<(u32, u32), String> {
        let mut lookups = lookups.into_iter();
        vmm::hold_endpoint_server(
            || {
                lookups
                    .next()
                    .expect("the endpoint is looked up at most twice")
            },
            |pid| Ok((!hold_exits).then_some(pid)),
        )
    }
    let failed = || Err(io::Error::other("lookup failed"));

    assert_eq!(hold(vec![Ok(Some(7)), Ok(Some(7))], false), Ok((7, 7)));
    // The server exited after the first lookup, and another process reused its ID before it was
    // held: once held, the endpoint names no process, or another one.
    for again in [Ok(None), Ok(Some(8))] {
        let reason = hold(vec![Ok(Some(7)), again], false).unwrap_err();
        assert!(reason.contains("stopped serving"), "{reason}");
    }
    let reason = hold(vec![Ok(Some(7)), failed()], false).unwrap_err();
    assert!(reason.contains("cannot be examined again"), "{reason}");
    // The server exited before it was held, or no process serves the endpoint.
    let reason = hold(vec![Ok(Some(7))], true).unwrap_err();
    assert!(
        reason.contains("exited before it could be verified"),
        "{reason}"
    );
    let reason = hold(vec![Ok(None)], false).unwrap_err();
    assert!(reason.contains("no process serves"), "{reason}");
    let reason = hold(vec![failed()], false).unwrap_err();
    assert!(reason.contains("cannot be examined"), "{reason}");
}
