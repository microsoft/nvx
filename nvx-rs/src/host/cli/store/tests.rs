use std::fs;
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::*;
use crate::host::cli::deadline::Deadline;

mod launch;
mod liveness;
mod recovery;

/// How long a test waits for a lock that it released to be free.
///
/// A process that a concurrent test spawns holds a copy of each descriptor of the test process
/// from its fork until it executes its program, and with it the lock of a file that a test had
/// open, for that moment.
const RELEASE_WAIT: Duration = Duration::from_secs(10);

/// Returns whether `free` holds before [`RELEASE_WAIT`] passes, checking it again every 25 ms
/// until then, the interval at which the CLI retries a lock.
fn released(mut free: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + RELEASE_WAIT;
    while !free() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(25));
    }
    true
}

/// Keeps process creation outside another thread's window of inheritable VMM handles.
#[cfg(windows)]
fn spawn_locked(command: &mut std::process::Command) -> std::io::Result<std::process::Child> {
    let _lock = platform::inherit::lock_spawns();
    command.spawn()
}

/// Makes `link` a junction to the directory `target`, which any user may create, with `cmd`'s
/// `mklink /J`, quoting both paths, since `cmd` splits its arguments at commas and semicolons.
///
/// `cmd` starts under a `SpawnLock`, so that it inherits no handle of a VMM that a concurrent test
/// spawns, such as the VMM's log, which it would hold until it exits.
#[cfg(windows)]
fn junction(link: &Path, target: &Path) {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut command = Command::new("cmd");
    command
        .raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            link.display(),
            target.display()
        ))
        .stdout(Stdio::null());
    let status = spawn_locked(&mut command).unwrap().wait().unwrap();
    assert!(status.success(), "cannot link {}", link.display());
}

/// Lets every user access `path` with `icacls`, through an entry that what the directory `path`
/// holds inherits as `inheritance` says, such as `(OI)(CI)`, or none for a file. `icacls` starts
/// under a `SpawnLock`, as `cmd` does in [`junction`].
#[cfg(windows)]
fn allow_everyone(path: &Path, inheritance: &str) {
    use std::process::{Command, Stdio};

    let mut command = Command::new("icacls");
    command
        .arg(path)
        .arg("/grant")
        .arg(format!("*S-1-1-0:{inheritance}(F)"))
        .stdout(Stdio::null());
    let status = spawn_locked(&mut command).unwrap().wait().unwrap();
    assert!(status.success(), "cannot share {}", path.display());
}

/// Returns a new state directory in a new temporary directory.
fn state() -> (tempfile::TempDir, StateDir) {
    let directory = tempfile::tempdir().unwrap();
    let state = StateDir::create(&directory.path().join("state")).unwrap();
    (directory, state)
}

/// Returns the code and the message of the error `exit`.
fn error(exit: Exit) -> (ErrorCode, String) {
    (exit.code().unwrap(), exit.message().unwrap().to_owned())
}

/// The error of a lock that another transition holds past the deadline.
fn busy() -> (ErrorCode, String) {
    (
        ErrorCode::SandboxBusy,
        "another lifecycle transition of the sandbox is still in progress".to_owned(),
    )
}

#[test]
fn state_directories_and_their_locks_are_private() {
    let (directory, state) = state();
    assert_eq!(StateDir::open(state.dir()).unwrap(), state);
    assert_eq!(StateDir::create(state.dir()).unwrap(), state);
    // The files of the five commands are in the directory itself.
    for file in StateFile::ALL {
        assert_eq!(state.path(file).parent(), Some(state.dir()), "{file:?}");
    }
    let lock = state.path(StateFile::Lock);
    drop(state.lock(Deadline::after(10.0)).unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode(state.dir()), 0o700);
        assert_eq!(mode(&lock), 0o600);
    }
    #[cfg(windows)]
    {
        assert_eq!(platform::check_store_dir(state.dir()).unwrap(), None);
        let file = fs::File::open(&lock).unwrap();
        assert_eq!(platform::private_file_problem(&file).unwrap(), None);
    }
    // Only existing directories open, and only a directory is a state directory.
    let missing = directory.path().join("missing");
    let (code, message) = error(StateDir::open(&missing).unwrap_err());
    assert_eq!(code, ErrorCode::NotFound);
    let opening = format!("cannot open the sandbox store {}: ", missing.display());
    assert!(message.starts_with(&opening), "{message}");
    let nested = missing.join("nested");
    let (code, message) = error(StateDir::create(&nested).unwrap_err());
    assert_eq!(code, ErrorCode::NotFound);
    let creating = format!("cannot create the sandbox store {}: ", nested.display());
    assert!(message.starts_with(&creating), "{message}");
    let file = directory.path().join("file");
    fs::write(&file, b"").unwrap();
    for exit in [
        StateDir::create(&file).unwrap_err(),
        StateDir::open(&file).unwrap_err(),
    ] {
        let expected = format!(
            "the sandbox store {} is not a plain directory",
            file.display()
        );
        assert_eq!(error(exit), (ErrorCode::Io, expected));
    }
    // A lock whose directory went, as deprovision removes it, finds none.
    fs::remove_file(&lock).unwrap();
    fs::remove_dir(state.dir()).unwrap();
    let (code, message) = error(state.lock(Deadline::after(10.0)).unwrap_err());
    assert_eq!(code, ErrorCode::NotFound);
    let locking = format!("cannot lock {}: ", lock.display());
    assert!(message.starts_with(&locking), "{message}");
}

#[cfg(target_os = "linux")]
#[test]
fn directories_that_others_may_access_are_refused() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    // No one else may reach a file in a directory that is private to the current user, whatever
    // the file's mode, so a private directory makes its lock private too.
    let (directory, state) = state();
    let shared = directory.path().join("shared");
    fs::create_dir(&shared).unwrap();
    for mode in [0o755, 0o777] {
        fs::set_permissions(&shared, fs::Permissions::from_mode(mode)).unwrap();
        for exit in [
            StateDir::create(&shared).unwrap_err(),
            StateDir::open(&shared).unwrap_err(),
        ] {
            let expected = format!(
                "the sandbox store {} is accessible to other users (mode {mode:03o})",
                shared.display()
            );
            assert_eq!(error(exit), (ErrorCode::Io, expected));
        }
        // The directory is refused rather than restricted: others could hold its files open.
        let now = fs::metadata(&shared).unwrap().permissions().mode() & 0o7777;
        assert_eq!(now, mode);
    }
    // A link to a state directory is none.
    let link = directory.path().join("link");
    symlink(state.dir(), &link).unwrap();
    for exit in [
        StateDir::create(&link).unwrap_err(),
        StateDir::open(&link).unwrap_err(),
    ] {
        let expected = format!(
            "the sandbox store {} is not a plain directory",
            link.display()
        );
        assert_eq!(error(exit), (ErrorCode::Io, expected));
    }
}

#[cfg(windows)]
#[test]
fn directories_that_others_may_access_are_refused() {
    let (directory, state) = state();
    let shared = directory.path().join("shared");
    fs::create_dir(&shared).unwrap();
    allow_everyone(&shared, "(OI)(CI)");
    for exit in [
        StateDir::create(&shared).unwrap_err(),
        StateDir::open(&shared).unwrap_err(),
    ] {
        let (code, message) = error(exit);
        assert_eq!(code, ErrorCode::Io);
        let prefix = format!("the sandbox store {} ", shared.display());
        assert!(message.starts_with(&prefix), "{message}");
        assert!(message.ends_with("allows other users"), "{message}");
    }
    // A junction to a state directory is none.
    let link = directory.path().join("link");
    junction(&link, state.dir());
    for exit in [
        StateDir::create(&link).unwrap_err(),
        StateDir::open(&link).unwrap_err(),
    ] {
        let expected = format!(
            "the sandbox store {} is not a plain directory",
            link.display()
        );
        assert_eq!(error(exit), (ErrorCode::Io, expected));
    }
}

#[test]
fn the_lock_excludes_other_holders_until_released_or_their_deadline() {
    let (_directory, state) = state();
    let held = state.lock(Deadline::after(10.0)).unwrap();
    let started = Instant::now();
    assert_eq!(error(state.lock(Deadline::after(0.1)).unwrap_err()), busy());
    assert!(started.elapsed() >= Duration::from_millis(100));
    // A deadline that has passed still tries once.
    assert_eq!(error(state.lock(Deadline::after(0.0)).unwrap_err()), busy());
    drop(held);
    assert!(released(|| state.lock(Deadline::after(0.0)).is_ok()));
    // A waiter takes the lock once its holder releases it.
    let held = state.lock(Deadline::after(10.0)).unwrap();
    let (sender, receiver) = mpsc::channel();
    let waiter = thread::spawn({
        let state = state.clone();
        move || {
            let locked = state.lock(Deadline::after(60.0)).map(drop);
            sender.send(()).unwrap();
            locked
        }
    });
    assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err());
    drop(held);
    waiter.join().unwrap().unwrap();
}

#[test]
fn a_lock_file_removed_while_waiting_locks_nothing() {
    let (_directory, state) = state();
    let held = state.lock(Deadline::after(10.0)).unwrap();
    let (locked, taken) = mpsc::channel();
    let (release, released_by_test) = mpsc::channel::<()>();
    let waiter = thread::spawn({
        let state = state.clone();
        move || {
            let lock = state.lock(Deadline::after(60.0)).unwrap();
            locked.send(()).unwrap();
            released_by_test.recv().unwrap();
            drop(lock);
        }
    });
    thread::sleep(Duration::from_millis(100));
    // The lock file goes while the waiter waits for it: the waiter then holds the lock of the
    // file that the path names, which a third transition finds held.
    fs::remove_file(state.path(StateFile::Lock)).unwrap();
    drop(held);
    taken.recv_timeout(Duration::from_secs(60)).unwrap();
    assert_eq!(error(state.lock(Deadline::after(0.1)).unwrap_err()), busy());
    release.send(()).unwrap();
    waiter.join().unwrap();
}

#[test]
fn lock_files_that_are_links_or_not_regular_files_are_refused_at_once() {
    let (_directory, state) = state();
    let path = state.path(StateFile::Lock);
    let not_regular = (
        ErrorCode::Io,
        format!("cannot lock {}: is not a regular file", path.display()),
    );
    // A lock taken through a link to a file that a start replaces, such as the capability, would
    // hold the replaced file, and the next transition would lock its replacement.
    let target = state.path(StateFile::Capability);
    fs::write(&target, b"capability").unwrap();
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&target, &path);
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_file(&target, &path);
    match linked {
        Ok(()) => {
            let exit = state.lock(Deadline::after(10.0)).unwrap_err();
            assert_eq!(error(exit), not_regular);
            assert!(fs::symlink_metadata(&path).unwrap().is_symlink());
            assert_eq!(fs::read(&target).unwrap(), b"capability");
            fs::remove_file(&path).unwrap();
        }
        // Windows lets only Developer Mode or a privilege create a symbolic link.
        Err(error) => eprintln!("skipped the symbolic link: {error}"),
    }
    // Any user may create a junction, which is a reparse point too.
    #[cfg(windows)]
    {
        let directory = _directory.path().join("target");
        fs::create_dir(&directory).unwrap();
        junction(&path, &directory);
        let (code, message) = error(state.lock(Deadline::after(10.0)).unwrap_err());
        assert_eq!(code, ErrorCode::Io);
        let locking = format!("cannot lock {}: ", path.display());
        assert!(message.starts_with(&locking), "{message}");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
        fs::remove_dir(&path).unwrap();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        use nix::sys::stat::Mode;

        // The open of a FIFO does not wait for a reader.
        nix::unistd::mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        let exit = state.lock(Deadline::after(10.0)).unwrap_err();
        assert_eq!(error(exit), not_regular);
        // A FIFO that has a reader opens, and is refused all the same.
        let _reader = fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits())
            .open(&path)
            .unwrap();
        let exit = state.lock(Deadline::after(10.0)).unwrap_err();
        assert_eq!(error(exit), not_regular);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn lock_files_get_back_the_permissions_that_the_umask_cleared() {
    use std::os::unix::fs::PermissionsExt;

    let (_directory, state) = state();
    let path = state.path(StateFile::Lock);
    fs::write(&path, b"").unwrap();
    for mode in [0o000, 0o400, 0o200] {
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        drop(state.lock(Deadline::after(10.0)).unwrap());
        let now = fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(now, 0o600, "{mode:03o}");
    }
}

#[cfg(windows)]
#[test]
fn lock_files_that_others_may_access_are_refused() {
    let (_directory, state) = state();
    drop(state.lock(Deadline::after(10.0)).unwrap());
    // A file's own access control list decides who may open it, whatever its directory allows.
    let path = state.path(StateFile::Lock);
    allow_everyone(&path, "");
    let expected = format!("the sandbox lock {} allows other users", path.display());
    let exit = state.lock(Deadline::after(10.0)).unwrap_err();
    assert_eq!(error(exit), (ErrorCode::Io, expected));
}
