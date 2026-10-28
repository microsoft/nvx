use std::fs;

use super::files::temporary_name;
use super::*;

#[test]
fn interrupted_writes_leave_temporary_files_that_only_their_removal_takes() {
    let directory = tempfile::tempdir().unwrap();
    let names = || {
        let mut names: Vec<String> = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    };
    let path = directory.path().join("config.json");
    write_json(&path, &serde_json::json!({ "written": true })).unwrap();
    assert_eq!(names(), ["config.json"]);
    // Replacements of the file that were interrupted left their temporary files, which other
    // files only resemble.
    let left = [0, 12345, u64::MAX].map(|number| temporary_name("config.json", number));
    let others = [
        ".config.json.tmp",
        ".config.json..tmp",
        ".config.json.012.tmp",
        ".config.json.+12.tmp",
        ".config.json.1a.tmp",
        ".config.json.18446744073709551616.tmp",
        ".config.json.1.tmp.tmp",
        ".xconfig.json.1.tmp",
        ".other.json.1.tmp",
        "config.json.1.tmp",
        ".tmpAbC123",
    ];
    for name in left.iter().map(String::as_str).chain(others) {
        fs::write(directory.path().join(name), b"{").unwrap();
    }
    remove_interrupted_writes(&path).unwrap();
    let mut kept: Vec<&str> = others.into_iter().chain(["config.json"]).collect();
    kept.sort_unstable();
    assert_eq!(names(), kept);
    // A directory that does not exist has none.
    remove_interrupted_writes(&directory.path().join("gone").join("config.json")).unwrap();
}

#[test]
fn readiness_failures_explain_images_without_workload_accounts() {
    let hint = "predates workload account creation";
    for ended in [SessionError::ProcessExited, SessionError::Closed] {
        assert!(readiness_failure(&ended, true).contains(hint));
        // Only a sandbox that needs the account suggests it.
        assert!(!readiness_failure(&ended, false).contains(hint));
    }
    for other in [
        SessionError::TimedOut,
        SessionError::Protocol("invalid record".to_owned()),
    ] {
        assert_eq!(readiness_failure(&other, true), other.to_string());
    }
}

#[test]
fn session_failures_have_the_codes_of_their_kinds_but_a_closed_endpoint() {
    assert_eq!(
        session_code(&SessionError::Closed),
        ErrorCode::EndpointClosed
    );
    assert_eq!(
        ErrorCode::EndpointClosed.kind(),
        session_kind(&SessionError::Closed)
    );
    for (error, code) in [
        (SessionError::TimedOut, ErrorCode::TimedOut),
        (SessionError::ProcessExited, ErrorCode::Exited),
        (SessionError::Reset, ErrorCode::Protocol),
        (SessionError::AuthenticationFailed, ErrorCode::Protocol),
        (
            SessionError::Protocol("bad".to_owned()),
            ErrorCode::Protocol,
        ),
        (SessionError::Io(io::Error::other("lost")), ErrorCode::Io),
    ] {
        assert_eq!(session_code(&error), code, "{error}");
    }
}

/// The instance's store, which exists on Linux and Windows hosts.
#[cfg(any(target_os = "linux", windows))]
mod store {
    use std::fs;

    use serde_json::json;

    use super::super::*;
    use crate::host::launch::PROCESS_COMMAND_LINE_LIMIT;
    use crate::host::store::{
        ArtifactIdentity, JOURNAL_NAME, LOCK_NAME, LOG_NAME, RUNTIME_NAME, RemovalError,
        RuntimeIdentity,
    };

    /// The configuration record of these tests' caller.
    const CONFIG: &str = "config.json";

    fn instance() -> (tempfile::TempDir, Instance) {
        let directory = tempfile::tempdir().unwrap();
        let instance = Instance::create(
            directory.path().join("vm"),
            "test",
            CallerFiles::new(CONFIG).unwrap(),
        )
        .unwrap();
        (directory, instance)
    }

    /// Returns the runtime identity of the launches of these tests.
    fn runtime() -> RuntimeIdentity {
        let artifact = |digit: &str| ArtifactIdentity::sha256(&digit.repeat(64)).unwrap();
        RuntimeIdentity::new(artifact("1"), artifact("2"), artifact("3"))
    }

    #[test]
    fn instances_that_record_no_vm_need_no_runtime_identity() {
        let (_directory, instance) = instance();
        let mut locked = instance.lock(None).unwrap();
        let found = locked
            .find(|| panic!("an instance that records no VM needs no runtime identity"))
            .unwrap();
        assert!(matches!(found, Found::Stopped), "{found:?}");
        drop(locked);
        // The instance's caller recorded no configuration, so its lock file went too.
        assert!(!instance.dir().join(LOCK_NAME).exists());
        let reopened =
            Instance::open(instance.dir(), instance.name(), instance.files().clone()).unwrap();
        assert_eq!(reopened, instance);
    }

    #[test]
    fn stopped_instances_are_removed_with_the_callers_files() {
        let (_directory, instance) = instance();
        for name in [CONFIG, LOG_NAME, RUNTIME_NAME] {
            fs::write(instance.dir().join(name), b"{}").unwrap();
        }
        // An instance whose store records a VM is not removed.
        let locked = instance.lock(None).unwrap();
        assert!(matches!(locked.remove(None), Err(RemovalError::Launched)));
        assert!(instance.dir().join(CONFIG).is_file());
        fs::remove_file(instance.dir().join(RUNTIME_NAME)).unwrap();
        // Nor is one whose caller declared no configuration record.
        let undeclared =
            Instance::open(instance.dir(), instance.name(), CallerFiles::none()).unwrap();
        let locked = undeclared.lock(None).unwrap();
        assert!(matches!(locked.remove(None), Err(RemovalError::Undeclared)));
        assert!(instance.dir().join(CONFIG).is_file());
        instance.lock(None).unwrap().remove(None).unwrap();
        assert!(!instance.dir().exists());
        // A transition that comes later finds the instance gone.
        let error = instance.lock(None).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
        let error =
            Instance::open(instance.dir(), instance.name(), CallerFiles::none()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
    }

    #[test]
    fn records_of_the_earlier_format_fail_naming_it() {
        // Before the store, instances kept runtime records and launch markers of format 3.
        for (name, record) in [
            (
                RUNTIME_NAME,
                json!({ "format": 3, "pid": 7, "startTime": 8, "endpoint": "e" }),
            ),
            (
                JOURNAL_NAME,
                json!({ "format": 3, "endpoint": "e", "process": { "pid": 7, "startTime": 8 } }),
            ),
        ] {
            let (_directory, instance) = instance();
            let path = instance.dir().join(name);
            fs::write(&path, record.to_string()).unwrap();
            let mut locked = instance.lock(None).unwrap();
            let error = locked.find(|| Ok(runtime())).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Io);
            assert!(
                error.message().contains("has the unsupported format 3"),
                "{error}"
            );
            assert!(
                error
                    .message()
                    .contains("stop the sandbox with the build that started it"),
                "{error}"
            );
            // Nothing is migrated or removed.
            assert!(path.is_file());
        }
    }

    #[test]
    fn launches_are_checked_before_the_instance_exists() {
        let directory = tempfile::tempdir().unwrap();
        let mut spec = LaunchSpec::new(
            Artifacts::new("openvmm", "vmlinux", "initramfs.cpio.gz"),
            crate::Hypervisor::Kvm,
        );
        let dir = directory.path().join("vm");
        check_launch(&dir, &spec).unwrap();
        spec.openvmm = directory
            .path()
            .join("o".repeat(PROCESS_COMMAND_LINE_LIMIT));
        let error = check_launch(&dir, &spec).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        assert!(!dir.exists());
    }
}
