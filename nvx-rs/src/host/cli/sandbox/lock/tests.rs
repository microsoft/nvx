use super::*;

fn state_dir(directory: &tempfile::TempDir) -> PathBuf {
    let state = directory.path().join("state");
    SandboxStore::create(&state).unwrap();
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
fn removals_keep_the_messages_of_deprovision() {
    let directory = tempfile::tempdir().unwrap();
    let state = state_dir(&directory);
    let dir = state.clone();
    let message = |error| {
        let exit = removal_error(error, &state, &dir);
        (exit.code().unwrap(), exit.message().unwrap().to_owned())
    };
    assert_eq!(
        message(RemovalError::Launched),
        (
            ErrorCode::SandboxRunning,
            "sandbox must be stopped before deprovision".to_owned()
        )
    );
    assert_eq!(
        message(RemovalError::Foreign(vec![
            "b.txt".to_owned(),
            "a.txt".to_owned()
        ])),
        (
            ErrorCode::ForeignFiles,
            "sandbox state directory contains files not owned by NVX: b.txt, a.txt".to_owned()
        )
    );
    assert_eq!(
        message(RemovalError::ProvisionedAgain),
        (
            ErrorCode::SandboxProvisioned,
            "sandbox was provisioned again while deprovision removed it".to_owned()
        )
    );
    let denied = || io::Error::from(io::ErrorKind::PermissionDenied);
    let config = state.join(CONFIG_NAME);
    for (path, shown) in [
        (dir.clone(), state.as_path()),
        (dir.join(CONFIG_NAME), config.as_path()),
    ] {
        let expected = format!("{shown:?}: {}", denied());
        assert_eq!(
            message(RemovalError::Io {
                path,
                error: denied()
            }),
            (ErrorCode::Io, expected)
        );
    }
    assert_eq!(
        message(RemovalError::TimedOut(denied())),
        (ErrorCode::TimedOut, format!("{state:?}: {}", denied()))
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
