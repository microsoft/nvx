use std::fs;

use super::*;
use crate::error::ErrorKind;
use crate::host::store::CLONE_FAILURE_NAME;
use crate::host::template::{LeaseId, StoreId};

/// Returns a directory private to the current user, as a sandbox's is, whose files are
/// private too, in a temporary directory, which removes it as it drops.
fn private_dir() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let private = directory.path().join("sandbox");
    assert_eq!(platform::create_owner_only_dir(&private).unwrap(), None);
    (directory, private)
}

/// Returns the record of a failure of a clone whose teardown ended as `teardown`.
fn failure(teardown: Teardown) -> FailureRecord {
    FailureRecord {
        store: StoreId::from_bytes([1; 16]),
        lease: LeaseId::from_bytes([2; 16]),
        template: TemplateReference {
            generation: StoreGeneration::new(NonZeroU64::new(7).unwrap()),
            key: Identity::from_bytes([3; 32]),
        },
        launch: LaunchId::try_from("4".repeat(32)).unwrap(),
        transaction: Some(TransactionId::from_bytes([5; 16])),
        stage: Stage::Activation,
        progress: Progress::Activating,
        evidence: Evidence::UnusableRoot,
        status: Some(54),
        outcome: Outcome::Request,
        teardown,
        code: ErrorCode::Exited,
        message: "the clone's VMM exited".to_owned(),
    }
}

#[test]
fn a_progress_log_records_each_step_once_with_the_steps_that_it_deferred() {
    let (_directory, private) = private_dir();
    let path = private.join(CLONE_PROGRESS_NAME);
    let launch = LaunchId::generate().unwrap();
    let boot = BootId::current().unwrap();
    let logged = |progress| {
        Some(Logged {
            progress,
            boot: Some(boot),
        })
    };
    // The log of an earlier launch goes, and so does the report of an earlier VM.
    write_private(&path, b"left").unwrap();
    let report = private.join(OUTCOME_NAME);
    write_private(&report, b"left").unwrap();
    let directory = platform::Directory::open(&private).unwrap();
    let mut log = ProgressLog::create(&directory, &private, &launch).unwrap();
    assert!(!report.exists());
    assert_eq!(
        read_progress(&path, &launch).unwrap(),
        logged(Progress::Launching)
    );
    // A step that another record holds waits for the next step that the log writes.
    log.advance(Progress::Spawned);
    assert_eq!(log.progress(), Progress::Spawned);
    assert_eq!(
        read_progress(&path, &launch).unwrap(),
        logged(Progress::Launching)
    );
    log.reach(Progress::Event).unwrap();
    log.advance(Progress::Acknowledged);
    log.reach(Progress::Activating).unwrap();
    // A step that the clone reached already is not recorded again.
    log.reach(Progress::Event).unwrap();
    log.advance(Progress::Spawned);
    assert_eq!(log.progress(), Progress::Activating);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        format!("{PROGRESS_MAGIC} 1 {launch} {boot}\nspawned\nevent\nacknowledged\nactivating\n")
    );
    assert_eq!(
        read_progress(&path, &launch).unwrap(),
        logged(Progress::Activating)
    );
}

#[test]
fn a_progress_log_replaces_an_earlier_one_only_in_the_directory_that_it_holds_open() {
    let (temporary, private) = private_dir();
    let launch = LaunchId::generate().unwrap();
    let directory = platform::Directory::open(&private).unwrap();
    write_private(&private.join(CLONE_PROGRESS_NAME), b"left").unwrap();
    write_private(&private.join(OUTCOME_NAME), b"left").unwrap();
    // The sandbox's path names another sandbox's directory by the time that the log is
    // created: the earlier log and report of the directory that the handle holds go, and the
    // other sandbox's stay, whose log fails the creation.
    let moved = temporary.path().join("moved");
    fs::rename(&private, &moved).unwrap();
    let other = temporary.path().join("other");
    assert_eq!(platform::create_owner_only_dir(&other).unwrap(), None);
    for name in [CLONE_PROGRESS_NAME, OUTCOME_NAME] {
        write_private(&other.join(name), b"the other's").unwrap();
    }
    fs::rename(&other, &private).unwrap();
    let error = ProgressLog::create(&directory, &private, &launch).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    for name in [CLONE_PROGRESS_NAME, OUTCOME_NAME] {
        assert!(!moved.join(name).exists(), "{name}");
        assert_eq!(
            fs::read(private.join(name)).unwrap(),
            b"the other's",
            "{name}"
        );
    }
}

#[test]
fn a_progress_log_that_a_crash_cut_short_is_read_and_one_of_another_launch_is_refused() {
    let (_directory, private) = private_dir();
    let path = private.join(CLONE_PROGRESS_NAME);
    let launch = LaunchId::generate().unwrap();
    assert_eq!(read_progress(&path, &launch).unwrap(), None);
    let (boot, other) = (BootId::current().unwrap(), BootId::from_bytes([0xee; 16]));
    let header = format!("{PROGRESS_MAGIC} 1 {launch} {boot}\n");
    // A last line that a crash cut short is not a step, nor is the first line, without which
    // the log names no boot. A log of another boot is read, with its boot, which a crash of
    // the host may have cut short.
    for (text, progress, written) in [
        (format!("{header}event\nackno"), Progress::Event, Some(boot)),
        (header[..10].to_owned(), Progress::Launching, None),
        (
            header[..header.len() - 1].to_owned(),
            Progress::Launching,
            None,
        ),
        (String::new(), Progress::Launching, None),
        (
            format!("{header}committed\n"),
            Progress::Committed,
            Some(boot),
        ),
        (
            format!("{PROGRESS_MAGIC} 1 {launch} {other}\nevent\n"),
            Progress::Event,
            Some(other),
        ),
    ] {
        write_private(&path, text.as_bytes()).unwrap();
        let read = read_progress(&path, &launch).unwrap().unwrap();
        assert_eq!((read.progress, read.boot), (progress, written), "{text:?}");
        // Only a log of the current boot tells for sure how far the clone got.
        assert_eq!(
            read.is_certain(Some(boot)),
            written == Some(boot),
            "{text:?}"
        );
        assert!(!read.is_certain(None), "{text:?}");
    }
    // Another launch's log, another format, a first line without a boot, and steps out of
    // their order, or that no log records, are refused.
    let other = LaunchId::generate().unwrap();
    for text in [
        format!("{PROGRESS_MAGIC} 1 {other} {boot}\nevent\n"),
        format!("{PROGRESS_MAGIC} 2 {launch} {boot}\n"),
        format!("{PROGRESS_MAGIC} 1 {launch}\n"),
        format!("{PROGRESS_MAGIC} 1 {launch} the-boot\n"),
        format!("{header}ready\nevent\n"),
        format!("{header}event\nevent\n"),
        format!("{header}planned\n"),
        format!("{header}launching\n"),
        format!("{header}event\nbogus\n"),
    ] {
        write_private(&path, text.as_bytes()).unwrap();
        let error = read_progress(&path, &launch).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{text:?}: {error}");
    }
}

#[test]
fn a_failure_record_is_read_as_it_was_written() {
    let (_directory, private) = private_dir();
    let path = private.join(CLONE_FAILURE_NAME);
    assert_eq!(read_failure(&path).unwrap(), None);
    let unresolved = Teardown::Unresolved("its VMM did not exit".to_owned());
    for teardown in [
        Teardown::NoVm,
        Teardown::Exited,
        Teardown::Terminated,
        unresolved,
    ] {
        let written = failure(teardown);
        write_failure(&path, &written).unwrap();
        assert_eq!(read_failure(&path).unwrap(), Some(written));
    }
    // Its JSON spells each field as the record's types do.
    let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "format": 1,
            "store": "01".repeat(16),
            "lease": "02".repeat(16),
            "generation": 7,
            "key": "03".repeat(32),
            "launch": "4".repeat(32),
            "transaction": "05".repeat(16),
            "stage": "activation",
            "progress": "activating",
            "evidence": "unusable-root",
            "status": 54,
            "outcome": "request",
            "teardown": "unresolved",
            "unresolved": "its VMM did not exit",
            "code": "exited",
            "message": "the clone's VMM exited",
        })
    );
    // A code that a later build records, which this one does not know, is still a failure.
    let mut later = value.clone();
    later["code"] = "a-later-code".into();
    write_private(&path, &serde_json::to_vec(&later).unwrap()).unwrap();
    assert_eq!(read_failure(&path).unwrap().unwrap().code, ErrorCode::Other);
}

#[test]
fn a_failure_record_is_in_place_when_its_directory_is_synced() {
    let (_directory, private) = private_dir();
    let path = private.join(CLONE_FAILURE_NAME);
    // Each write syncs the record's directory once the record is in place, so that the sync
    // makes the record's rename durable.
    let synced = std::cell::RefCell::new(Vec::new());
    let sync = |dir: &Path| -> Result<()> {
        synced
            .borrow_mut()
            .push((dir.to_owned(), read_failure(&path).unwrap()));
        sync_dir(dir)
    };
    let (pending, exited) = (failure(Teardown::Pending), failure(Teardown::Exited));
    write_failure_with(&path, &pending, &sync).unwrap();
    write_failure_with(&path, &exited, &sync).unwrap();
    assert_eq!(
        synced.into_inner(),
        [
            (private.clone(), Some(pending)),
            (private.clone(), Some(exited))
        ]
    );
    // A sync that fails fails the write, whose record is in place, though maybe not durably.
    let refused = |_: &Path| -> Result<()> { Err(Error::io("the test's sync failed")) };
    let terminated = failure(Teardown::Terminated);
    let error = write_failure_with(&path, &terminated, &refused).unwrap_err();
    assert_eq!(error.message(), "the test's sync failed");
    assert_eq!(read_failure(&path).unwrap(), Some(terminated));
}

#[test]
fn a_failure_record_keeps_the_ends_of_texts_too_long_for_a_read_to_take() {
    // A text keeps its first and its last characters, which it counts as characters.
    assert_eq!(bounded("abcdef", 6), "abcdef");
    assert_eq!(bounded("abcdefg", 6), "ab…efg");
    assert_eq!(bounded("ééééééé", 4), "é…éé");
    let (_directory, private) = private_dir();
    let path = private.join(CLONE_FAILURE_NAME);
    // JSON takes six bytes for each of these control characters, the most that a character
    // takes, so that each text, whole, would take more than a read takes.
    let filler = "\u{1}".repeat(64 * 1024);
    let mut written = failure(Teardown::Unresolved(format!("why{filler}so")));
    written.message = format!("start{filler}end");
    write_failure(&path, &written).unwrap();
    assert!(fs::metadata(&path).unwrap().len() <= MAX_FAILURE_BYTES);
    let read = read_failure(&path).unwrap().unwrap();
    assert_eq!(read, kept(written));
    assert_eq!(read.message.chars().count(), MAX_MESSAGE_CHARS);
    assert!(read.message.starts_with("start\u{1}") && read.message.ends_with("\u{1}end"));
    let Teardown::Unresolved(reason) = &read.teardown else {
        panic!("{:?}", read.teardown);
    };
    assert_eq!(reason.chars().count(), MAX_REASON_CHARS);
    assert!(reason.starts_with("why\u{1}") && reason.ends_with("\u{1}so"));
}

#[test]
fn a_failure_record_of_another_format_or_a_malformed_one_is_refused() {
    let (_directory, private) = private_dir();
    let path = private.join(CLONE_FAILURE_NAME);
    write_failure(&path, &failure(Teardown::Exited)).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let changed = |change: &dyn Fn(&mut serde_json::Value)| {
        let mut value = value.clone();
        change(&mut value);
        write_private(&path, &serde_json::to_vec(&value).unwrap()).unwrap();
        read_failure(&path).unwrap_err()
    };
    let error = changed(&|value| value["format"] = 2.into());
    assert_eq!(error.code(), ErrorCode::FormatUnsupported, "{error}");
    type Change = fn(&mut serde_json::Value);
    let malformed: [Change; 6] = [
        |value| value["unresolved"] = "a reason of a resolved teardown".into(),
        |value| value["teardown"] = "unresolved".into(),
        |value| value["teardown"] = "vanished".into(),
        |value| value["generation"] = 0.into(),
        |value| value["transaction"] = "05".into(),
        |value| value["extra"] = true.into(),
    ];
    for change in malformed {
        let error = changed(&change);
        assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    }
    write_private(&path, b"{").unwrap();
    assert_eq!(read_failure(&path).unwrap_err().kind(), ErrorKind::Io);
}

#[test]
fn a_failure_record_of_another_clone_than_the_one_of_the_lease_is_refused() {
    let (_directory, private) = private_dir();
    let path = private.join(CLONE_FAILURE_NAME);
    let written = failure(Teardown::Exited);
    let lease = Lease::new(
        written.lease,
        written.store,
        written.template.generation,
        written.template.key,
        private.clone(),
        written.launch.clone(),
    );
    assert_eq!(read_failure_of(&path, &lease).unwrap(), None);
    write_failure(&path, &written).unwrap();
    assert_eq!(
        read_failure_of(&path, &lease).unwrap(),
        Some(written.clone())
    );
    // A record of the failure of a clone of another store, of another lease, of another
    // generation, or of another key, or of another launch, tells nothing of the lease's clone,
    // though `read_failure` reads it.
    let generation = StoreGeneration::new(NonZeroU64::new(8).unwrap());
    let launch = LaunchId::try_from("6".repeat(32)).unwrap();
    let others = [
        (
            FailureRecord {
                store: StoreId::from_bytes([6; 16]),
                ..written.clone()
            },
            "the template store",
        ),
        (
            FailureRecord {
                lease: LeaseId::from_bytes([6; 16]),
                ..written.clone()
            },
            "the lease",
        ),
        (
            FailureRecord {
                template: TemplateReference {
                    generation,
                    ..written.template
                },
                ..written.clone()
            },
            "the leased generation",
        ),
        (
            FailureRecord {
                template: TemplateReference {
                    key: Identity::from_bytes([6; 32]),
                    ..written.template
                },
                ..written.clone()
            },
            "the key of the leased generation",
        ),
        (
            FailureRecord {
                launch: launch.clone(),
                ..written.clone()
            },
            "the launch",
        ),
        (
            FailureRecord {
                lease: LeaseId::from_bytes([6; 16]),
                template: TemplateReference {
                    generation,
                    ..written.template
                },
                launch,
                ..written.clone()
            },
            "the lease, the leased generation and the launch",
        ),
    ];
    for (other, differences) in others {
        write_failure(&path, &other).unwrap();
        assert_eq!(read_failure(&path).unwrap(), Some(other));
        let error = read_failure_of(&path, &lease).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{error}");
        assert!(
            error
                .to_string()
                .ends_with(&format!("they differ in {differences}")),
            "{error}"
        );
    }
    // A record that cannot be read fails as it does for `read_failure`.
    write_private(&path, b"{").unwrap();
    assert_eq!(
        read_failure_of(&path, &lease).unwrap_err().kind(),
        ErrorKind::Io
    );
}

#[test]
fn the_line_of_openvmm_s_fatal_error_is_read_from_the_end_of_its_log() {
    let (_directory, private) = private_dir();
    let path = private.join("openvmm.log");
    assert_eq!(read_fatal(&path), None);
    // OpenVMM's warnings may name a time ABI code too; the line that leads its fatal error,
    // which the error's chain follows, is the one that tells why it exited.
    let fatal = "fatal error: [E_TSC_RATE_TOLERANCE] failed to launch vm worker";
    let log = format!(
        "NVX-CPU-PROFILE-FALLBACK: [E_PROFILE_UNSUPPORTED] a warning\r\n{fatal}\r\n\r\n\
             Caused by:\r\n    0: [E_TSC_RATE_TOLERANCE] the destination's TSC rate\r\n"
    );
    fs::write(&path, &log).unwrap();
    assert_eq!(read_fatal(&path).as_deref(), Some(fatal));
    // The last line of a long log is read, kept to its first characters.
    let long = format!("fatal error: {}", "x".repeat(2 * MAX_FATAL_CHARS));
    let filler = "a line of the VMM's log\n".repeat(2 * FATAL_BLOCK_BYTES as usize / 24);
    fs::write(
        &path,
        format!("fatal error: an earlier one\n{filler}{long}\n"),
    )
    .unwrap();
    let read = read_fatal(&path).unwrap();
    assert_eq!(read.chars().count(), MAX_FATAL_CHARS);
    assert!(long.starts_with(&read));
    // A log without a fatal error, or one that only names it, holds none, however long.
    fs::write(&path, "the VMM exited\nwarning: fatal error: quoted\n").unwrap();
    assert_eq!(read_fatal(&path), None);
    fs::write(
        &path,
        format!("{filler}warning: fatal error: quoted\n{filler}"),
    )
    .unwrap();
    assert_eq!(read_fatal(&path), None);
}

#[test]
fn the_line_of_openvmm_s_fatal_error_is_read_before_a_chain_of_any_length() {
    let (_directory, private) = private_dir();
    let path = private.join("openvmm.log");
    let block = FATAL_BLOCK_BYTES as usize;
    // A failed CPU check names every difference that it found, in a cause that may take
    // several blocks of the log, after the line, which may be the log's first.
    let fatal = "fatal error: [E_CPU_SURFACE] failed to launch vm worker";
    let differences = "CPUID 0x1 ECX differs; ".repeat(3 * block / 23);
    for before in ["", "a line of the VMM's log\n"] {
        let log = format!("{before}{fatal}\n\nCaused by:\n    {differences}\n");
        fs::write(&path, &log).unwrap();
        assert_eq!(read_fatal(&path).as_deref(), Some(fatal));
    }
    // A cause that names a forward that OpenVMM cannot bind follows the line after it.
    let forward = "failed to create socket for port forward (Tcp 127.0.0.1:18080)";
    let log = format!("{fatal}\n\nCaused by:\n    0: {differences}\n    1: {forward}\n");
    fs::write(&path, &log).unwrap();
    assert_eq!(read_fatal(&path), Some(format!("{fatal}: {forward}")));
    // The line is read wherever a block starts around it: after the newline before the line,
    // at the line's start, within its prefix, or after it.
    let filler = "a line of the VMM's log\n".repeat(2 * block / 24);
    let prefix = failure::FATAL_PREFIX.len();
    for from_end in block - 2..=block + prefix + 2 {
        let rest = "y".repeat(from_end - fatal.len() - 2);
        let log = format!("{filler}{fatal}\n{rest}\n");
        assert_eq!(log.len() - log.find(fatal).unwrap(), from_end);
        fs::write(&path, &log).unwrap();
        assert_eq!(read_fatal(&path).as_deref(), Some(fatal), "{from_end}");
    }
}

#[test]
fn the_cause_that_names_a_forward_that_openvmm_cannot_bind_follows_the_fatal_line() {
    let (_directory, private) = private_dir();
    let path = private.join("openvmm.log");
    let fatal = "fatal error: failed to launch vm worker";
    let forward = "failed to create socket for port forward (Tcp 127.0.0.1:18080)";
    let expected = format!("{fatal}: {forward}");
    // OpenVMM lists the causes of its error's chain after the line, numbered when there are
    // several, and alone otherwise.
    for chain in [
        format!(
            "    0: failed to resolve the network endpoint\n    1: {forward}\n    2: Address \
                 already in use (os error 98)\n"
        ),
        format!("    {forward}\n"),
    ] {
        let log = format!("a line of the VMM's log\n{fatal}\n\nCaused by:\n{chain}");
        fs::write(&path, &log).unwrap();
        assert_eq!(
            read_fatal(&path).as_deref(),
            Some(expected.as_str()),
            "{log}"
        );
    }
    // A line that names the forward itself keeps its text alone.
    let named = format!("fatal error: {forward}");
    fs::write(&path, format!("{named}\n\nCaused by:\n    {forward}\n")).unwrap();
    assert_eq!(read_fatal(&path).as_deref(), Some(named.as_str()));
    // A forward that a line before the fatal error names is no cause of it.
    let log = format!("warning: {forward}\n{fatal}\n\nCaused by:\n    0: another cause\n");
    fs::write(&path, log).unwrap();
    assert_eq!(read_fatal(&path).as_deref(), Some(fatal));
}
