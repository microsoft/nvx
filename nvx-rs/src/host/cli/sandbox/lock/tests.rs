use super::*;

fn state_dir(directory: &tempfile::TempDir) -> PathBuf {
    let state = directory.path().join("state");
    StateDir::create(&state).unwrap();
    state
}

#[test]
fn the_lock_excludes_other_holders_until_released() {
    let directory = tempfile::tempdir().unwrap();
    let state = state_dir(&directory);
    std::fs::write(state.join(CONFIG_NAME), "{}").unwrap();
    let held = LifecycleLock::acquire(&state, 10.0, false).unwrap();
    let started = std::time::Instant::now();
    let error = LifecycleLock::acquire(&state, 0.1, false).err().unwrap();
    assert!(started.elapsed() >= std::time::Duration::from_millis(100));
    assert_eq!(error.code(), Some(ErrorCode::SandboxBusy));
    assert_eq!(
        error.message(),
        Some("another lifecycle transition of the sandbox is still in progress")
    );
    drop(held);
    drop(LifecycleLock::acquire(&state, 10.0, false).unwrap());
}

#[test]
fn missing_state_directories_are_created_by_provision_alone() {
    let directory = tempfile::tempdir().unwrap();
    let state = state_dir(&directory);
    let missing = state.join("missing");
    let error = LifecycleLock::acquire(&missing, 1.0, false).err().unwrap();
    assert_eq!(error.code(), Some(ErrorCode::SandboxNotProvisioned));
    assert_eq!(
        error.message().unwrap(),
        format!(
            "sandbox state path is not a plain directory: {}",
            missing.display()
        )
    );
    // A provision creates the directory that deprovision removed meanwhile, and its parents.
    let nested = missing.join("nested");
    drop(LifecycleLock::acquire(&nested, 1.0, true).unwrap());
    assert!(nested.is_dir());
}

#[cfg(unix)]
#[test]
fn directories_that_others_may_access_are_refused() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let state = state_dir(&directory);
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();
    let error = LifecycleLock::acquire(&state, 1.0, false).err().unwrap();
    assert_eq!(
        error.message().unwrap(),
        format!(
            "the sandbox store {} is accessible to other users (mode 755)",
            state.display()
        )
    );
}

#[test]
fn removals_refuse_live_records_and_keep_foreign_files() {
    let directory = tempfile::tempdir().unwrap();
    let state = state_dir(&directory);
    std::fs::write(state.join(CONFIG_NAME), "{}").unwrap();
    for record in [StateFile::Journal, StateFile::Runtime] {
        let lock = LifecycleLock::acquire(&state, 10.0, false).unwrap();
        let path = state.join(record.name());
        std::fs::write(&path, "{}").unwrap();
        let error = lock.remove(10.0).unwrap_err();
        assert_eq!(error.code(), Some(ErrorCode::SandboxRunning));
        assert_eq!(
            error.message(),
            Some("sandbox must be stopped before deprovision")
        );
        assert!(path.exists());
        assert!(state.join(CONFIG_NAME).exists());
        std::fs::remove_file(path).unwrap();
    }
    std::fs::write(state.join("foreign.txt"), "keep").unwrap();
    let lock = LifecycleLock::acquire(&state, 10.0, false).unwrap();
    let error = lock.remove(10.0).unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::ForeignFiles));
    assert_eq!(
        std::fs::read_to_string(state.join("foreign.txt")).unwrap(),
        "keep"
    );
    assert!(!state.join(CONFIG_NAME).exists());
    assert!(!state.join(StateFile::Lock.name()).exists());
}

#[test]
fn removal_cleans_owned_temporaries_without_touching_a_neighbour() {
    let directory = tempfile::tempdir().unwrap();
    let state = state_dir(&directory);
    let neighbour = directory.path().join("neighbour");
    StateDir::create(&neighbour).unwrap();
    std::fs::write(neighbour.join(CONFIG_NAME), "neighbour").unwrap();
    std::fs::write(state.join(CONFIG_NAME), "{}").unwrap();
    for name in [
        ".runtime.json.42.tmp",
        ".config.json.0123456789abcdef0123456789abcdef.tmp",
        ".tmpAb12Cd",
        "openvmm.log",
        "outcome.json",
        "control.capability",
    ] {
        std::fs::write(state.join(name), "owned").unwrap();
    }
    let lock = LifecycleLock::acquire(&state, 10.0, false).unwrap();
    lock.remove(10.0).unwrap();
    assert!(!state.exists());
    assert_eq!(
        std::fs::read_to_string(neighbour.join(CONFIG_NAME)).unwrap(),
        "neighbour"
    );
}

#[test]
fn removals_remove_the_state_directory() {
    let directory = tempfile::tempdir().unwrap();
    let state = state_dir(&directory);
    std::fs::write(state.join(CONFIG_NAME), "{}").unwrap();
    let lock = LifecycleLock::acquire(&state, 10.0, false).unwrap();
    lock.remove(10.0).unwrap();
    assert!(!state.exists());
    let error = LifecycleLock::acquire(&state, 1.0, false).err().unwrap();
    assert_eq!(error.code(), Some(ErrorCode::SandboxNotProvisioned));
    assert_eq!(
        error.message().unwrap(),
        format!(
            "sandbox state path is not a plain directory: {}",
            state.display()
        )
    );
}
