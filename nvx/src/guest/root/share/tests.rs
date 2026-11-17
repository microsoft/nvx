use super::*;

/// Returns the refusal of `target`, if it is refused.
fn refusal(target: &str) -> Option<ShareRefusal> {
    match check_target(Path::new(target)) {
        Ok(()) => None,
        Err(Error::Share(refusal)) => Some(refusal),
        Err(error) => panic!("{target}: {error}"),
    }
}

#[test]
fn targets_are_canonical_absolute_paths_outside_the_runtime_s_mounts() {
    for target in [
        "/workspace",
        "/opt/hostedtoolcache",
        "/a/b/c",
        "/a/...",
        "/.hidden",
        "/etc/app",
        "/etc/machine-ids",
        "/etc/resolv.conf.d",
        "/run/app",
        "/run/nvx/state",
        "/run/nvx/timer",
        "/procs",
        "/devices",
        "/.nvx-agents",
    ] {
        assert_eq!(refusal(target), None, "{target}");
    }
    for target in ["workspace", "", "./a"] {
        assert!(
            matches!(refusal(target), Some(ShareRefusal::Relative(_))),
            "{target}"
        );
    }
    for target in [
        "/", "//a", "/a//b", "/a/", "/a/.", "/a/./b", "/a/..", "/../a", "/a\0b",
    ] {
        assert!(
            matches!(refusal(target), Some(ShareRefusal::NotCanonical(_))),
            "{target}"
        );
    }
    // A target names a path below the root, which is never longer than a path.
    let longest = format!("/{}", "a".repeat(mappings::MAX_PATH_BYTES - 1));
    assert_eq!(refusal(&longest), None);
    assert!(matches!(
        refusal(&format!("{longest}a")),
        Some(ShareRefusal::NotCanonical(_))
    ));
    for target in ["/a\\b", "/a=b", "/a,b", "/a\"b"] {
        assert!(
            matches!(refusal(target), Some(ShareRefusal::ReservedCharacter(_))),
            "{target}"
        );
    }
    for target in [
        "/proc",
        "/proc/self",
        "/sys",
        "/sys/fs/cgroup",
        "/dev",
        "/dev/shm",
        "/.nvx-agent",
        "/.nvx-agent/tool",
        "/etc",
        "/etc/machine-id",
        "/etc/machine-id/x",
        "/etc/resolv.conf",
        "/etc/resolv.conf/x",
        "/run",
        "/run/nvx",
        "/run/nvx/time",
        "/run/nvx/time/state",
    ] {
        assert!(
            matches!(refusal(target), Some(ShareRefusal::Reserved(_))),
            "{target}"
        );
    }
}

#[test]
fn tags_and_child_names_are_plain_names() {
    for tag in ["microvm", "a_b-C9"] {
        assert!(check_tag(tag).is_ok(), "{tag}");
    }
    for tag in ["", "a.b", "a b", "a/b", "é"] {
        assert!(check_tag(tag).is_err(), "{tag}");
    }
    for name in ["0", "a.b_C-9", "..."] {
        assert!(check_name(name).is_ok(), "{name}");
    }
    for name in ["", ".", "..", "a/b", "a b", "a,b"] {
        assert!(check_name(name).is_err(), "{name}");
    }
}

#[test]
fn the_export_s_directory_is_created_private_whatever_the_umask() {
    for umask in [0o000, 0o022, 0o077, 0o777] {
        // The test's own directory is created with the test's umask, in which this thread
        // may still create entries.
        let directory = tempfile::tempdir().unwrap();
        let hostfs = directory.path().join("hostfs");
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o7777;
        sys::tests::with_thread_umask(umask, || create_private_directory(&hostfs)).unwrap();
        assert_eq!(mode(&hostfs), HOSTFS_DIRECTORY_MODE, "{umask:o}");
        // A private directory that is there already, such as this one, serves as it is.
        sys::tests::with_thread_umask(umask, || create_private_directory(&hostfs)).unwrap();
        assert_eq!(mode(&hostfs), HOSTFS_DIRECTORY_MODE, "{umask:o}");
        // One that its group or others may enter is refused, and keeps its mode.
        fs::set_permissions(&hostfs, fs::Permissions::from_mode(0o750)).unwrap();
        let error =
            sys::tests::with_thread_umask(umask, || create_private_directory(&hostfs)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{umask:o}");
        assert_eq!(mode(&hostfs), 0o750, "{umask:o}");
    }
    // Anything else there is refused, a symbolic link to a directory included.
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("file");
    fs::write(&file, "").unwrap();
    let link = directory.path().join("link");
    std::os::unix::fs::symlink(directory.path(), &link).unwrap();
    for path in [file, link] {
        let error = create_private_directory(&path).unwrap_err();
        assert_eq!(
            error.raw_os_error(),
            Some(Errno::ENOTDIR as i32),
            "{path:?}"
        );
    }
}

#[test]
fn a_directory_is_private_to_its_owner_when_its_group_and_others_have_no_permission() {
    // The owner's permissions, the file type, and the set-ID and sticky bits grant no one
    // else access.
    for mode in [0o700, 0o500, 0o000, 0o40700, 0o2700, 0o1700] {
        check_private(0, mode, 0).unwrap();
        check_private(1000, mode, 1000).unwrap();
    }
    for mode in [0o710, 0o701, 0o750, 0o705, 0o755, 0o777, 0o40711] {
        let error = check_private(0, mode, 0).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{mode:o}");
    }
    assert_eq!(
        check_private(0, 0o40750, 0).unwrap_err().to_string(),
        "the directory there already is not private to user 0: user 0 owns it with mode 0750"
    );
    // Another user's directory is refused, however private.
    let error = check_private(1000, 0o40700, 0).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(
        error.to_string(),
        "the directory there already is not private to user 0: user 1000 owns it with mode \
             0700"
    );
}

#[test]
fn an_export_s_mode_is_the_access_of_its_mount() {
    assert_eq!(Access::from(ExportMode::ReadOnly), Access::ReadOnly);
    assert_eq!(Access::from(ExportMode::ReadWrite), Access::ReadWrite);
}
