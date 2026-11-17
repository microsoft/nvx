use super::*;
use crate::host::cli::deadline::Deadline;
use crate::host::cli::store::{StateDir, StateFile};
use crate::host::platform;
use std::fs;

fn state() -> (tempfile::TempDir, StateDir) {
    let directory = tempfile::tempdir().unwrap();
    let state = StateDir::create(&directory.path().join("state")).unwrap();
    (directory, state)
}

fn admit(state: &StateDir) -> Result<Option<ExecLock>> {
    let locked = StateDir::open(state.dir())?.lock(Deadline::after(1.0))?;
    ExecLock::try_acquire(&locked)
}

#[test]
fn admission_is_private_nonblocking_and_keeps_its_file_on_release() {
    let (_directory, state) = state();
    let held = admit(&state).unwrap().unwrap();
    let path = state.dir().join(NAME);
    let identity = platform::file_id(&path).unwrap();
    assert_eq!(platform::private_file_problem(&held.file).unwrap(), None);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert!(admit(&state).unwrap().is_none());
    let other = StateDir::create(&_directory.path().join("other")).unwrap();
    drop(admit(&other).unwrap().unwrap());
    drop(held);
    let _next = admit(&state).unwrap().unwrap();
    assert_eq!(platform::file_id(&path).unwrap(), identity);
}

#[test]
fn a_replaced_lifecycle_lock_refuses_admission_before_file_creation() {
    let (_directory, state) = state();
    let old = state.lock(Deadline::after(1.0)).unwrap();
    fs::rename(state.path(StateFile::Lock), state.dir().join("old.lock")).unwrap();
    let current = state.lock(Deadline::after(1.0)).unwrap();
    assert_eq!(
        ExecLock::try_acquire(&old).unwrap_err().code(),
        Some(ErrorCode::Io)
    );
    assert!(!state.dir().join(NAME).exists());
    let _held = ExecLock::try_acquire(&current).unwrap().unwrap();
    drop(old);
    assert!(ExecLock::try_acquire(&current).unwrap().is_none());
}

#[cfg(unix)]
#[test]
fn a_link_to_an_already_locked_exec_file_is_refused() {
    let (_directory, state) = state();
    let _held = admit(&state).unwrap().unwrap();
    let path = state.dir().join(NAME);
    let target = state.dir().join("old.lock");
    fs::rename(&path, &target).unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert_eq!(admit(&state).unwrap_err().code(), Some(ErrorCode::Io));
    assert!(path.is_symlink());
}

#[test]
fn non_regular_exec_lock_entries_are_refused() {
    let (_directory, state) = state();
    let path = state.dir().join(NAME);
    fs::create_dir(&path).unwrap();
    assert_eq!(admit(&state).unwrap_err().code(), Some(ErrorCode::Io));
    assert!(path.is_dir());
    fs::remove_dir(&path).unwrap();
    #[cfg(unix)]
    {
        let target = state.dir().join("target");
        fs::write(&target, "keep").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert_eq!(admit(&state).unwrap_err().code(), Some(ErrorCode::Io));
        assert!(path.is_symlink());
        assert_eq!(fs::read_to_string(target).unwrap(), "keep");
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};

        let target = state.dir().join("target");
        fs::create_dir(&target).unwrap();
        let mut command = Command::new("cmd");
        command
            .raw_arg(format!(
                "/C mklink /J \"{}\" \"{}\"",
                path.display(),
                target.display()
            ))
            .stdout(Stdio::null());
        let child = {
            let _spawn = platform::inherit::lock_spawns();
            command.spawn().unwrap()
        };
        assert!(child.wait_with_output().unwrap().status.success());
        assert_eq!(admit(&state).unwrap_err().code(), Some(ErrorCode::Io));
        assert_eq!(fs::read_dir(target).unwrap().count(), 0);
    }
}

#[cfg(windows)]
#[test]
fn exec_lock_files_accessible_to_other_users_are_refused_without_restriction() {
    use std::process::{Command, Stdio};

    let (_directory, state) = state();
    drop(admit(&state).unwrap().unwrap());
    let path = state.dir().join(NAME);
    let mut command = Command::new("icacls");
    command
        .arg(&path)
        .args(["/grant", "*S-1-1-0:(F)"])
        .stdout(Stdio::null());
    let child = {
        let _spawn = platform::inherit::lock_spawns();
        command.spawn().unwrap()
    };
    assert!(child.wait_with_output().unwrap().status.success());
    let problem = platform::private_file_problem(&File::open(&path).unwrap()).unwrap();
    assert!(problem.is_some());
    assert_eq!(admit(&state).unwrap_err().code(), Some(ErrorCode::Io));
    assert_eq!(
        platform::private_file_problem(&File::open(&path).unwrap()).unwrap(),
        problem
    );
}

#[cfg(target_os = "linux")]
#[test]
fn private_directories_protect_exec_locks_without_changing_the_store_file_policy() {
    use std::os::unix::fs::PermissionsExt;

    let (_directory, state) = state();
    drop(admit(&state).unwrap().unwrap());
    let path = state.dir().join(NAME);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    drop(admit(&state).unwrap().unwrap());
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o644
    );
    fs::set_permissions(state.dir(), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(admit(&state).unwrap_err().code(), Some(ErrorCode::Io));
    assert_eq!(
        fs::metadata(state.dir()).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o644
    );
}
