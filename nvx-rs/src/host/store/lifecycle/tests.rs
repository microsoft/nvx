//! Tests of the lifecycle locks, their release, and the removal of a store, by threads of one
//! process, and by transitions in processes of their own, which [`transition_role`] runs.

use std::cell::RefCell;
use std::env;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use super::*;
use crate::host::platform::inherit::spawn_command;
use crate::host::store::{
    CAPABILITY_NAME, CLONE_ACTIVATION_NAME, CLONE_FAILURE_NAME, CLONE_PROGRESS_NAME,
    CLONE_SCRATCH_NAME, CONSOLE_NAME, LOG_NAME, OUTCOME_NAME, RESTORE_READY_NAME, SOCKET_NAME,
};
use crate::host::test_support::released;

/// The configuration record of these tests' caller.
const CONFIG: &str = "config.json";
/// Another file of these tests' caller.
const BOOT: &str = "boot.sock";
/// A name that NFS gives a removed lock file that a transition still holds open.
const RENAMED: &str = ".nfs00000000000000010000";
/// The directory of the sandbox whose lock [`transition_role`] takes.
const ROLE_DIR: &str = "NVX_LIFECYCLE_TEST_DIR";
/// The lock that [`transition_role`] takes: `provision` or `existing`.
const ROLE_LOCK: &str = "NVX_LIFECYCLE_TEST_LOCK";
/// The line that [`transition_role`] prints before it waits for the lock.
const WAITING: &str = "nvx-lifecycle-test-waiting";
/// The start of the line in which [`transition_role`] prints how its transition ended.
const OUTCOME: &str = "nvx-lifecycle-test-outcome ";

/// Returns the files of these tests' caller.
fn files() -> CallerFiles {
    CallerFiles::new(CONFIG).unwrap().with(BOOT).unwrap()
}

/// Returns the deadline `seconds` from now.
fn within(seconds: u64) -> Option<Instant> {
    Some(Instant::now() + Duration::from_secs(seconds))
}

/// Returns the names of the entries of `dir`, sorted.
fn entries_of(dir: &Path) -> Vec<String> {
    let mut names = entries(dir).unwrap();
    names.sort();
    names
}

/// Returns `names` as sorted strings.
fn sorted(names: &[&str]) -> Vec<String> {
    let mut names: Vec<String> = names.iter().map(|&name| name.to_owned()).collect();
    names.sort();
    names
}

/// Provisions a sandbox in a fresh temporary directory, and returns the directory of its store.
fn sandbox() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path().join("sandbox");
    let locked = SandboxStore::lock_for_provision(&dir, &files(), None).unwrap();
    assert!(!dir.join(CONFIG).exists());
    fs::write(dir.join(CONFIG), b"{}").unwrap();
    drop(locked);
    (directory, dir)
}

/// Takes the lock of the existing sandbox in `dir`, and starts its removal.
fn remove(dir: &Path) -> std::result::Result<Option<Lingering>, RemovalError> {
    SandboxStore::lock_existing(dir, &files(), None)
        .unwrap()
        .remove()
}

/// Removes the sandbox in `dir` as [`LockedStore::remove`] does while the lock file of another
/// transition, which NFS renamed as the transition still held it, keeps the directory.
fn remove_lingering(dir: &Path) -> Lingering {
    fs::write(dir.join(RENAMED), b"").unwrap();
    remove(dir)
        .unwrap()
        .expect("a lingering lock file keeps the directory")
}

/// Takes the lock of the existing sandbox in `dir` until `deadline` and releases it, and returns
/// whether the sandbox was provisioned.
fn transition(dir: &Path, deadline: Option<Instant>) -> Result<bool> {
    let locked = SandboxStore::lock_existing(dir, &files(), deadline)?;
    Ok(locked.dir().join(CONFIG).exists())
}

/// A transition in a process of its own, which [`transition_role`] runs.
struct Transition {
    /// The process.
    child: Child,
    /// Its standard output.
    output: BufReader<ChildStdout>,
}

impl Transition {
    /// Starts a transition that takes the lock `lock` of the sandbox in `dir`, and returns once
    /// it is about to wait for the lock.
    fn start(dir: &Path, lock: &str) -> Self {
        let mut command = Command::new(env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "host::store::lifecycle::tests::transition_role",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ROLE_DIR, dir)
            .env(ROLE_LOCK, lock)
            .stdin(Stdio::null())
            .stdout(Stdio::piped());
        let mut child = spawn_command(&mut command).unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        while line.trim_end() != WAITING {
            line.clear();
            let read = output.read_line(&mut line).unwrap();
            assert_ne!(
                read, 0,
                "the transition ended before it waited for the lock"
            );
        }
        Self { child, output }
    }

    /// Returns whether the transition still runs.
    fn runs(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    /// Waits for the transition to end, and returns how it ended, as [`transition_role`] prints
    /// it.
    fn outcome(mut self) -> String {
        let mut rest = String::new();
        self.output.read_to_string(&mut rest).unwrap();
        assert!(self.child.wait().unwrap().success(), "{rest}");
        rest.lines()
            .find_map(|line| line.strip_prefix(OUTCOME))
            .expect("the transition printed its outcome")
            .to_owned()
    }
}

impl Drop for Transition {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A transition in a process of its own, which [`Transition::start`] runs: it takes the lock that
/// [`ROLE_LOCK`] names of the sandbox in [`ROLE_DIR`] within 30 s, and prints how it ended:
/// `provisioned` if it found the configuration record, `unprovisioned` if it did not, which a
/// provision then writes, and `gone` if the sandbox no longer exists.
#[test]
#[ignore = "a transition that the lifecycle tests run in a process of its own"]
fn transition_role() {
    let (Some(dir), Ok(lock)) = (env::var_os(ROLE_DIR), env::var(ROLE_LOCK)) else {
        return;
    };
    // The test harness has printed the test's name without ending the line.
    println!("\n{WAITING}");
    let locked = match lock.as_str() {
        "provision" => SandboxStore::lock_for_provision(&dir, &files(), within(30)),
        "existing" => SandboxStore::lock_existing(&dir, &files(), within(30)),
        _ => panic!("unknown lock {lock}"),
    };
    let outcome = match locked {
        Ok(locked) if locked.dir().join(CONFIG).exists() => "provisioned",
        Ok(locked) => {
            if lock == "provision" {
                fs::write(locked.dir().join(CONFIG), b"{}").unwrap();
            }
            "unprovisioned"
        }
        Err(error) if error.kind() == ErrorKind::NotFound => "gone",
        Err(error) => panic!("{error}"),
    };
    println!("{OUTCOME}{outcome}");
}

#[test]
fn lifecycle_locks_exclude_other_holders_until_released() {
    let (_directory, dir) = sandbox();
    let held = SandboxStore::lock_existing(&dir, &files(), None).unwrap();
    let started = Instant::now();
    let deadline = Some(Instant::now() + Duration::from_millis(100));
    let error = SandboxStore::lock_existing(&dir, &files(), deadline).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TimedOut, "{error}");
    assert!(started.elapsed() >= Duration::from_millis(100));
    let error = SandboxStore::lock_for_provision(&dir, &files(), Some(Instant::now())).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TimedOut, "{error}");
    // A waiter takes the lock once its holder releases it.
    let (sender, receiver) = mpsc::channel();
    let waiter = thread::spawn({
        let dir = dir.clone();
        move || {
            let locked = SandboxStore::lock_existing(&dir, &files(), within(30)).map(drop);
            sender.send(()).unwrap();
            locked
        }
    });
    assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err());
    drop(held);
    waiter.join().unwrap().unwrap();
    assert!(released(|| {
        SandboxStore::lock_existing(&dir, &files(), Some(Instant::now())).is_ok()
    }));
    // A provisioned directory keeps its lock file.
    assert_eq!(entries_of(&dir), sorted(&[CONFIG, LOCK_NAME]));
}

#[test]
fn released_locks_leave_no_lock_file_in_unprovisioned_directories() {
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path().join("sandbox");
    // A provision that records nothing leaves the directory that it created empty, and so does
    // a transition of a sandbox that was never provisioned.
    drop(SandboxStore::lock_for_provision(&dir, &files(), None).unwrap());
    assert!(entries_of(&dir).is_empty());
    drop(SandboxStore::lock_existing(&dir, &files(), None).unwrap());
    assert!(entries_of(&dir).is_empty());
    // A directory that does not exist holds no sandbox: only a provision creates it, below a
    // parent that exists.
    let missing = directory.path().join("missing");
    let error = SandboxStore::lock_existing(&missing, &files(), None).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
    assert!(!missing.exists());
    drop(SandboxStore::lock_for_provision(&missing, &files(), None).unwrap());
    assert!(missing.is_dir());
    let orphan = directory.path().join("absent").join("sandbox");
    let error = SandboxStore::lock_for_provision(&orphan, &files(), None).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
    assert!(!directory.path().join("absent").exists());
    // A caller that keeps no configuration record keeps the lock file, as the store's own lock
    // does.
    drop(SandboxStore::lock_existing(&dir, &CallerFiles::none(), None).unwrap());
    assert_eq!(entries_of(&dir), [LOCK_NAME]);
    drop(SandboxStore::open(&dir).unwrap().lock(None).unwrap());
    assert_eq!(entries_of(&dir), [LOCK_NAME]);
}

#[test]
fn a_holder_removes_only_its_own_lock_file_on_release() {
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path().join("sandbox");
    // A holder that removed its lock file, as a removal does while it holds the lock, leaves the
    // new lock file of a transition that took the lock meanwhile.
    let mut held = SandboxStore::lock_for_provision(&dir, &files(), None).unwrap();
    fs::remove_file(dir.join(LOCK_NAME)).unwrap();
    held.lock_removed = true;
    let other = SandboxStore::lock_existing(&dir, &files(), within(10)).unwrap();
    drop(held);
    assert_eq!(entries_of(&dir), [LOCK_NAME]);
    drop(other);
    assert!(entries_of(&dir).is_empty());
    // So does a holder whose lock file another replaced.
    let held = SandboxStore::lock_existing(&dir, &files(), None).unwrap();
    fs::remove_file(dir.join(LOCK_NAME)).unwrap();
    fs::write(dir.join(LOCK_NAME), b"").unwrap();
    drop(held);
    assert_eq!(entries_of(&dir), [LOCK_NAME]);
}

#[test]
fn directories_that_others_may_access_are_refused() {
    let (_directory, dir) = sandbox();
    #[cfg(unix)]
    let problem = {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        "is accessible to other users (mode 755)"
    };
    #[cfg(windows)]
    let problem = {
        platform::set_access(&dir, "D:(A;OICI;FA;;;WD)").unwrap();
        "allows other users"
    };
    for error in [
        SandboxStore::lock_existing(&dir, &files(), None).unwrap_err(),
        SandboxStore::lock_for_provision(&dir, &files(), None).unwrap_err(),
    ] {
        assert_eq!(
            error.message(),
            format!("the sandbox store {} {problem}", dir.display())
        );
    }
}

#[test]
fn stale_directories_hold_no_sandbox_and_keep_no_lock_file() {
    let directory = tempfile::tempdir().unwrap();
    // A directory that was never provisioned, and those that an interrupted removal left: empty,
    // or with a lock file and the temporary file of an interrupted replacement.
    let never = SandboxStore::create(directory.path().join("never")).unwrap();
    let emptied = SandboxStore::create(directory.path().join("emptied")).unwrap();
    let left = SandboxStore::create(directory.path().join("left")).unwrap();
    fs::write(left.dir().join(LOCK_NAME), b"").unwrap();
    fs::write(left.dir().join(".runtime.json.1.tmp"), b"{").unwrap();
    for store in [&never, &emptied, &left] {
        let locked = SandboxStore::lock_existing(store.dir(), &files(), None).unwrap();
        assert!(!locked.dir().join(CONFIG).exists());
        drop(locked);
        assert!(!store.dir().join(LOCK_NAME).exists());
    }
    // A removal of such a directory finds nothing of a sandbox, and removes it.
    assert!(remove(left.dir()).unwrap().is_none());
    assert!(!left.dir().exists());
}

#[test]
fn a_removal_removes_the_store_the_callers_files_and_the_directory() {
    let (_directory, dir) = sandbox();
    for name in [
        LOG_NAME,
        OUTCOME_NAME,
        CONSOLE_NAME,
        CAPABILITY_NAME,
        SOCKET_NAME,
        CLONE_BINDING_NAME,
        CLONE_SCRATCH_NAME,
        CLONE_ACTIVATION_NAME,
        CLONE_PROGRESS_NAME,
        CLONE_FAILURE_NAME,
        RESTORE_READY_NAME,
        BOOT,
        ".runtime.json.12345.tmp",
        ".sandbox.lock.7.tmp",
        ".tmpAbC123",
    ] {
        fs::write(dir.join(name), b"left").unwrap();
    }
    assert!(remove(&dir).unwrap().is_none());
    assert!(!dir.exists());
    // A transition that comes later finds no sandbox, and creates nothing.
    let error = SandboxStore::lock_existing(&dir, &files(), None).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
    assert!(!dir.exists());
}

#[test]
fn a_removal_refuses_foreign_entries_and_keeps_them() {
    let (_directory, dir) = sandbox();
    fs::write(dir.join(LOG_NAME), b"log").unwrap();
    fs::write(dir.join(BOOT), b"").unwrap();
    // Entries that resemble the store's files and temporary files, or the caller's, are foreign,
    // as are the caller's own temporary files, which it removes itself.
    let foreign = [
        "notes.txt",
        ".config.json.1.tmp",
        ".runtime.json.1a.tmp",
        "outcome.json.old",
        ".tmpAbC1234",
        "directory",
    ];
    for name in &foreign[..5] {
        fs::write(dir.join(name), b"kept").unwrap();
    }
    fs::create_dir(dir.join("directory")).unwrap();
    // A removed file that NFS keeps while a handle holds it is not foreign: it goes as the
    // handle closes.
    fs::write(dir.join(RENAMED), b"").unwrap();
    let RemovalError::Foreign(mut names) = remove(&dir).unwrap_err() else {
        panic!("foreign entries were not refused");
    };
    names.sort();
    assert_eq!(names, sorted(&foreign));
    // The store's files and the caller's went first, and the lock file as the lock was released,
    // so the sandbox is gone and the foreign entries remain, in the directory.
    let mut remaining = foreign.to_vec();
    remaining.push(RENAMED);
    assert_eq!(entries_of(&dir), sorted(&remaining));
    assert_eq!(fs::read(dir.join("notes.txt")).unwrap(), b"kept");
    let error = Error::from(RemovalError::Foreign(vec!["a".to_owned(), "b".to_owned()]));
    assert_eq!(error.kind(), ErrorKind::Other);
    assert_eq!(
        error.message(),
        "the sandbox's directory holds entries that are neither the store's nor its caller's: a, b"
    );
    // Once they are gone, the directory goes too.
    for name in &foreign[..5] {
        fs::remove_file(dir.join(name)).unwrap();
    }
    fs::remove_dir(dir.join("directory")).unwrap();
    fs::remove_file(dir.join(RENAMED)).unwrap();
    assert!(remove(&dir).unwrap().is_none());
    assert!(!dir.exists());
}

#[test]
fn a_removal_is_refused_while_a_vm_or_a_launch_is_recorded() {
    for name in [JOURNAL_NAME, RUNTIME_NAME] {
        let (_directory, dir) = sandbox();
        fs::write(dir.join(name), b"{}").unwrap();
        fs::write(dir.join(LOG_NAME), b"log").unwrap();
        fs::write(dir.join(CLONE_BINDING_NAME), b"{}").unwrap();
        assert!(
            matches!(remove(&dir), Err(RemovalError::Launched)),
            "{name}"
        );
        // Nothing was removed, not even the clone binding of the VM that may run, and the
        // provisioned sandbox keeps its lock file.
        assert_eq!(
            entries_of(&dir),
            sorted(&[CLONE_BINDING_NAME, CONFIG, LOCK_NAME, LOG_NAME, name])
        );
    }
}

/// Writes the files that a clone keeps of its launch in the sandbox `dir`, its binding included,
/// and the file of the scratch device that an earlier version left for a clone in RAM.
fn write_clone_files(dir: &Path) {
    for name in [
        CLONE_BINDING_NAME,
        CLONE_SCRATCH_NAME,
        CLONE_ACTIVATION_NAME,
        CLONE_PROGRESS_NAME,
        CLONE_FAILURE_NAME,
        RESTORE_READY_NAME,
    ] {
        fs::write(dir.join(name), b"{}").unwrap();
    }
}

#[test]
fn a_removal_makes_the_absence_of_the_clones_files_durable_before_it_removes_anything_else() {
    let (_directory, dir) = sandbox();
    write_clone_files(&dir);
    for name in [LOG_NAME, BOOT] {
        fs::write(dir.join(name), b"{}").unwrap();
    }
    let locked = SandboxStore::lock_existing(&dir, &files(), None).unwrap();
    let synced = RefCell::new(Vec::new());
    locked
        .remove_contents_with(&|| {}, &|directory| {
            // The directory that the sync makes durable holds the sandbox's lock file.
            let lock = directory.entry_id(LOCK_NAME).unwrap();
            synced.borrow_mut().push((lock, entries_of(&dir)));
            Ok(())
        })
        .unwrap();
    // A crash after the sync, even once the directory or one of its ancestors is gone, cannot
    // bring the binding or the clone's files back, and one before the configuration record goes
    // leaves a provisioned sandbox, whose removal can be retried.
    assert_eq!(
        synced.into_inner(),
        [(
            Some(platform::file_id(&dir.join(LOCK_NAME)).unwrap()),
            sorted(&[BOOT, CONFIG, LOCK_NAME, LOG_NAME])
        )]
    );
    assert_eq!(entries_of(&dir), sorted(&[LOCK_NAME]));
    assert!(locked.remove_directory().unwrap().is_none());
    assert!(!dir.exists());
}

#[test]
fn a_removal_that_cannot_make_the_absence_of_the_clones_files_durable_keeps_the_sandbox() {
    let (_directory, dir) = sandbox();
    write_clone_files(&dir);
    fs::write(dir.join(LOG_NAME), b"{}").unwrap();
    let locked = SandboxStore::lock_existing(&dir, &files(), None).unwrap();
    let error = locked
        .remove_contents_with(&|| {}, &|_| Err(io::Error::other("the sync failed")))
        .unwrap_err();
    assert!(
        matches!(&error, RemovalError::Io { path, .. } if path == locked.dir()),
        "{error}"
    );
    assert_eq!(Error::from(error).kind(), ErrorKind::Io);
    drop(locked);
    // Only the clone's files went, so the sandbox is still provisioned, and its removal can be
    // retried.
    assert_eq!(entries_of(&dir), sorted(&[CONFIG, LOCK_NAME, LOG_NAME]));
    assert!(remove(&dir).unwrap().is_none());
    assert!(!dir.exists());
}

/// Provisions a sandbox in `dir`, whose parent exists.
fn provision(dir: &Path) {
    let locked = SandboxStore::lock_for_provision(dir, &files(), None).unwrap();
    fs::write(dir.join(CONFIG), b"{}").unwrap();
    drop(locked);
}

/// Links `link` to the directory `target`: a symbolic link on Unix, and a junction on Windows,
/// which any user may create.
fn link_dir(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    crate::host::test_support::junction(link, target);
}

/// Makes the link `link`, which [`link_dir`] made, name the directory `target` instead.
fn relink(target: &Path, link: &Path) {
    #[cfg(unix)]
    fs::remove_file(link).unwrap();
    // Removing a junction removes the link alone.
    #[cfg(windows)]
    fs::remove_dir(link).unwrap();
    link_dir(target, link);
}

/// Provisions the sandboxes `one/sandbox` and `two/sandbox` in `root`, each holding a clone
/// binding and the files that a clone keeps of its launch, links `root/link` to `one`, and returns
/// `one`, `two`, and the link.
fn linked_sandboxes(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let (one, two, link) = (root.join("one"), root.join("two"), root.join("link"));
    for parent in [&one, &two] {
        fs::create_dir(parent).unwrap();
        let sandbox = parent.join("sandbox");
        provision(&sandbox);
        write_clone_files(&sandbox);
    }
    link_dir(&one, &link);
    (one, two, link)
}

/// The entries of a provisioned sandbox that [`linked_sandboxes`] made, with `more`, sorted.
fn linked_entries(more: &[&str]) -> Vec<String> {
    let mut names = vec![
        CLONE_ACTIVATION_NAME,
        CLONE_BINDING_NAME,
        CLONE_FAILURE_NAME,
        CLONE_PROGRESS_NAME,
        CLONE_SCRATCH_NAME,
        CONFIG,
        LOCK_NAME,
        RESTORE_READY_NAME,
    ];
    names.extend_from_slice(more);
    sorted(&names)
}

#[test]
fn a_removal_unbinds_the_sandbox_whose_lock_it_holds_whatever_its_path_names_meanwhile() {
    let directory = tempfile::tempdir().unwrap();
    let (one, two, link) = linked_sandboxes(directory.path());
    // The other sandbox records a launch, so its VM may run, and its binding and the files of its
    // clone's launch must stay.
    fs::write(two.join("sandbox").join(JOURNAL_NAME), b"{}").unwrap();
    let locked = SandboxStore::lock_existing(link.join("sandbox"), &files(), None).unwrap();
    // The link names the other sandbox's parent from the checks to the binding's removal, and
    // this one's again from the sync on.
    locked
        .remove_contents_with(&|| relink(&two, &link), &|_| {
            relink(&one, &link);
            Ok(())
        })
        .unwrap();
    assert_eq!(entries_of(&one.join("sandbox")), [LOCK_NAME]);
    assert_eq!(
        entries_of(&two.join("sandbox")),
        linked_entries(&[JOURNAL_NAME])
    );
    assert!(locked.remove_directory().unwrap().is_none());
    assert!(!one.join("sandbox").exists());
}

#[test]
fn a_removal_through_a_path_that_names_another_sandbox_meanwhile_removes_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let (one, two, link) = linked_sandboxes(directory.path());
    let locked = SandboxStore::lock_existing(link.join("sandbox"), &files(), None).unwrap();
    let dir = locked.dir().to_owned();
    // Once the lock is taken, the link comes to name the other sandbox's parent, whose lock file
    // is not this lock's.
    relink(&two, &link);
    let error = locked.remove().unwrap_err();
    assert!(
        matches!(&error, RemovalError::Io { path, .. } if *path == dir),
        "{error}"
    );
    assert_eq!(Error::from(error).kind(), ErrorKind::Io);
    for parent in [&one, &two] {
        assert_eq!(entries_of(&parent.join("sandbox")), linked_entries(&[]));
    }
}

#[test]
fn lingering_lock_files_keep_the_directory_until_they_go() {
    let (_directory, dir) = sandbox();
    let mut lingering = remove_lingering(&dir);
    assert_eq!(lingering.dir(), dir);
    assert_eq!(entries_of(&dir), [RENAMED]);
    // The removal released its lock, and the sandbox is gone. A transition takes the lock of a
    // new lock file, which keeps the directory too, and finds no configuration record.
    let waiting = SandboxStore::lock_existing(&dir, &files(), within(10)).unwrap();
    assert!(!dir.join(CONFIG).exists());
    let Err(RemovalError::TimedOut(error)) = lingering.wait(Some(Instant::now())) else {
        panic!("the directory went while lock files kept it");
    };
    assert!(directory_not_empty(&error), "{error}");
    fs::remove_file(dir.join(RENAMED)).unwrap();
    assert!(matches!(
        lingering.wait(Some(Instant::now())),
        Err(RemovalError::TimedOut(_))
    ));
    // The transition removes its lock file as it releases its lock, which empties the directory.
    let removed = thread::spawn(move || lingering.wait(within(30)));
    thread::sleep(Duration::from_millis(50));
    drop(waiting);
    removed.join().unwrap().unwrap();
    assert!(!dir.exists());
}

#[test]
fn a_lingering_removal_reports_what_appears_in_the_directory() {
    // A provision that takes the lock that the removal released provisions the directory again.
    let (_directory, dir) = sandbox();
    let mut lingering = remove_lingering(&dir);
    let provision = SandboxStore::lock_for_provision(&dir, &files(), within(10)).unwrap();
    fs::write(dir.join(CONFIG), b"{}").unwrap();
    drop(provision);
    fs::remove_file(dir.join(RENAMED)).unwrap();
    assert!(matches!(
        lingering.wait(within(10)),
        Err(RemovalError::ProvisionedAgain)
    ));
    assert_eq!(entries_of(&dir), sorted(&[CONFIG, LOCK_NAME]));
    // Other entries are foreign, sorted.
    let mut lingering = remove_lingering(&dir);
    for name in ["zebra.txt", "apple.txt"] {
        fs::write(dir.join(name), b"").unwrap();
    }
    let Err(RemovalError::Foreign(names)) = lingering.wait(within(10)) else {
        panic!("foreign entries were not refused");
    };
    assert_eq!(names, ["apple.txt", "zebra.txt"]);
    // The path may name another directory once the removal's is gone, which is not its to
    // remove.
    for name in [RENAMED, "zebra.txt", "apple.txt"] {
        fs::remove_file(dir.join(name)).unwrap();
    }
    fs::remove_dir(&dir).unwrap();
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join(RENAMED), b"").unwrap();
    lingering.wait(Some(Instant::now())).unwrap();
    assert_eq!(entries_of(&dir), [RENAMED]);
}

#[test]
fn a_lingering_removal_finds_the_configuration_record_by_its_path() {
    // A provision by a caller that declares the configuration record in another case writes the
    // removal's record on a file system that ignores case, as Windows' do, and another file on
    // one that does not.
    let (_directory, dir) = sandbox();
    let mut lingering = remove_lingering(&dir);
    let upper = CONFIG.to_ascii_uppercase();
    let files = CallerFiles::new(upper.as_str()).unwrap();
    let provision = SandboxStore::lock_for_provision(&dir, &files, within(10)).unwrap();
    fs::write(dir.join(&upper), b"{}").unwrap();
    drop(provision);
    fs::remove_file(dir.join(RENAMED)).unwrap();
    let ignores_case = dir.join(CONFIG).exists();
    match lingering.wait(within(10)) {
        Err(RemovalError::ProvisionedAgain) => assert!(ignores_case),
        Err(RemovalError::Foreign(names)) => {
            assert!(
                !ignores_case,
                "a provision in another case was taken for a foreign file"
            );
            assert_eq!(names, std::slice::from_ref(&upper));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(entries_of(&dir), sorted(&[upper.as_str(), LOCK_NAME]));
}

#[test]
fn a_removal_that_another_finishes_succeeds_and_keeps_what_replaced_the_directory() {
    // Another removal, which takes the lock of a new lock file once this one has removed its own,
    // finds the emptied directory, and removes it first.
    let removed_by_another = |dir: &Path| {
        let other = SandboxStore::lock_existing(dir, &files(), within(10)).unwrap();
        assert!(other.remove().unwrap().is_none());
    };
    let (_directory, dir) = sandbox();
    let locked = SandboxStore::lock_existing(&dir, &files(), None).unwrap();
    locked.remove_contents().unwrap();
    let lingering = locked
        .remove_directory_after(|| removed_by_another(&dir))
        .unwrap();
    assert!(lingering.is_none());
    assert!(!dir.exists());
    // A directory that replaces it then stays, even an empty one.
    let (_directory, dir) = sandbox();
    let locked = SandboxStore::lock_existing(&dir, &files(), None).unwrap();
    locked.remove_contents().unwrap();
    let lingering = locked
        .remove_directory_after(|| {
            removed_by_another(&dir);
            fs::create_dir(&dir).unwrap();
        })
        .unwrap();
    assert!(lingering.is_none());
    assert!(entries_of(&dir).is_empty());
}

#[test]
fn a_lingering_removal_keeps_an_empty_directory_that_replaced_its_own() {
    let (_directory, dir) = sandbox();
    let mut lingering = remove_lingering(&dir);
    // Another removal finishes this one, and the path is created again, still empty.
    fs::remove_file(dir.join(RENAMED)).unwrap();
    fs::remove_dir(&dir).unwrap();
    fs::create_dir(&dir).unwrap();
    lingering.wait(Some(Instant::now())).unwrap();
    assert!(dir.is_dir());
    assert!(entries_of(&dir).is_empty());
}

#[test]
fn only_the_directory_that_the_path_still_names_is_removed() {
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path().join("sandbox");
    fs::create_dir(&dir).unwrap();
    let (identity, held) = hold_directory(&dir).unwrap();
    // Another removal removes the held directory, and another one replaces it, which stays.
    fs::remove_dir(&dir).unwrap();
    fs::create_dir(&dir).unwrap();
    assert!(!remove_held_directory(&dir, identity).unwrap());
    assert!(dir.is_dir());
    drop(held);
    let (identity, _held) = hold_directory(&dir).unwrap();
    assert!(remove_held_directory(&dir, identity).unwrap());
    assert!(!dir.exists());
    // A path that names nothing fails as its check does.
    let error = remove_held_directory(&dir, identity).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn directories_are_identified_as_their_seals_identify_them() {
    use crate::host::seal::{FileSeal, open_metadata_only};

    // On Windows, by the 128-bit file ID of `FILE_ID_INFO`, which tells the directories of ReFS
    // volumes apart where their 64-bit file index may not.
    let directory = tempfile::tempdir().unwrap();
    let seal = FileSeal::of(&open_metadata_only(directory.path()).unwrap()).unwrap();
    let expected = (seal.device(), seal.file_id());
    assert_eq!(identity_of(directory.path()).unwrap(), expected);
    assert_eq!(hold_directory(directory.path()).unwrap().0, expected);
}

#[test]
fn a_removal_needs_the_configuration_record() {
    let (_directory, dir) = sandbox();
    fs::write(dir.join(LOG_NAME), b"log").unwrap();
    // Without it, a transition that waits through the removal could not tell that the sandbox
    // is gone, so neither a lifecycle lock without one nor the store's own lock removes it.
    let undeclared = SandboxStore::lock_existing(&dir, &CallerFiles::none(), None).unwrap();
    assert!(matches!(undeclared.remove(), Err(RemovalError::Undeclared)));
    let own = SandboxStore::open(&dir).unwrap().lock(None).unwrap();
    assert!(matches!(own.remove(), Err(RemovalError::Undeclared)));
    assert_eq!(entries_of(&dir), sorted(&[CONFIG, LOCK_NAME, LOG_NAME]));
    let error = Error::from(RemovalError::Undeclared);
    assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
}

#[test]
fn a_provision_creates_again_a_directory_that_a_removal_deletes_while_it_is_created() {
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path().join("sandbox");
    // A concurrent removal deletes the directory after it is created, before it is checked.
    let mut attempts = 0;
    let locked = SandboxStore::provision_lock(
        &dir,
        &files(),
        within(10),
        |dir| {
            attempts += 1;
            if attempts == 1 {
                SandboxStore::create(dir).unwrap();
                fs::remove_dir(dir).unwrap();
                return SandboxStore::open(dir);
            }
            SandboxStore::create(dir)
        },
        SandboxStore::take_lifecycle_lock,
    )
    .unwrap();
    assert_eq!(attempts, 2);
    assert!(!locked.dir().join(CONFIG).exists());
    assert_eq!(entries_of(&dir), [LOCK_NAME]);
    drop(locked);
    // A missing parent fails at once.
    let orphan = directory.path().join("missing").join("sandbox");
    let error = SandboxStore::lock_for_provision(&orphan, &files(), within(10)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
    // A directory that keeps disappearing fails once the deadline passes.
    let gone = directory.path().join("gone");
    let mut attempts = 0;
    let deadline = Some(Instant::now() + Duration::from_millis(200));
    let error = SandboxStore::provision_lock(
        &dir,
        &files(),
        deadline,
        |_| {
            attempts += 1;
            SandboxStore::open(&gone)
        },
        SandboxStore::take_lifecycle_lock,
    )
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TimedOut, "{error}");
    assert!(attempts > 2, "{attempts}");
}

#[test]
fn locks_whose_lock_file_keeps_being_replaced_keep_their_deadline() {
    // Transitions that keep removing their lock files as they release their locks replace the
    // lock file each time that a waiter takes its lock. The waiter takes the lock again at once
    // the first time, and after a pause when that repeats, until its deadline.
    let (_directory, dir) = sandbox();
    for provision in [false, true] {
        for (deadline, most) in [
            (Some(Instant::now()), 2),
            (Some(Instant::now() + Duration::from_millis(200)), 20),
        ] {
            let mut taken = 0;
            let replaced = |_: &SandboxStore, _: Option<Instant>| {
                taken += 1;
                Ok(Taken::Replaced)
            };
            let error = if provision {
                SandboxStore::provision_lock(
                    &dir,
                    &files(),
                    deadline,
                    |dir| SandboxStore::create(dir),
                    replaced,
                )
            } else {
                let store = SandboxStore::open(&dir).unwrap();
                SandboxStore::existing_lock(store, &files(), deadline, replaced)
            }
            .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::TimedOut, "{error}");
            assert!((2..=most).contains(&taken), "{provision} {most}: {taken}");
        }
    }
}

#[test]
fn a_provision_that_waits_through_a_removal_provisions_the_sandbox_again() {
    let (_directory, dir) = sandbox();
    let locked = SandboxStore::lock_existing(&dir, &files(), None).unwrap();
    let (ready, waiting) = mpsc::channel();
    let provision = thread::spawn({
        let dir = dir.clone();
        move || {
            ready.send(()).unwrap();
            let locked = SandboxStore::lock_for_provision(&dir, &files(), within(30)).unwrap();
            let provisioned = dir.join(CONFIG).exists();
            fs::write(dir.join(CONFIG), b"{}").unwrap();
            drop(locked);
            provisioned
        }
    });
    waiting.recv().unwrap();
    thread::sleep(Duration::from_millis(100));
    // The removal completes before the provision takes the lock, which creates the directory
    // again, or, should the provision open the lock file between the removals of the lock file
    // and of the directory, reports the provision. Where a removed file stays while a handle
    // holds it, as on NFS, the handle with which the provision waits for the removed lock file
    // keeps the directory instead, which then goes once the provision closes it, before the
    // provision creates the directory again.
    match locked.remove().unwrap() {
        None => {}
        Some(mut lingering) => {
            assert!(matches!(
                lingering.wait(within(30)),
                Ok(()) | Err(RemovalError::ProvisionedAgain)
            ));
        }
    }
    assert!(!provision.join().unwrap());
    assert_eq!(entries_of(&dir), sorted(&[CONFIG, LOCK_NAME]));
}

#[test]
fn transitions_that_wait_through_a_removal_find_the_sandbox_gone() {
    let (_directory, dir) = sandbox();
    let locked = SandboxStore::lock_existing(&dir, &files(), None).unwrap();
    let (ready, waiting) = mpsc::channel();
    let waiters: Vec<_> = [None, within(30)]
        .into_iter()
        .map(|deadline| {
            let (dir, ready) = (dir.clone(), ready.clone());
            thread::spawn(move || {
                ready.send(()).unwrap();
                transition(&dir, deadline)
            })
        })
        .collect();
    for _ in &waiters {
        waiting.recv().unwrap();
    }
    thread::sleep(Duration::from_millis(100));
    let lingering = locked.remove().unwrap();
    let outcomes: Vec<_> = waiters
        .into_iter()
        .map(|waiter| waiter.join().unwrap())
        .collect();
    // The waiters wait for the lock of the removed lock file, and find the directory gone. A
    // waiter that started late may open the lock file between the removals of the lock file and
    // of the directory, which creates a new one, and find no sandbox: the directory then lingers
    // until it releases that lock, unless it released it before the removal of the directory.
    for outcome in outcomes {
        match outcome {
            Ok(provisioned) => assert!(!provisioned),
            Err(error) => assert_eq!(error.kind(), ErrorKind::NotFound, "{error}"),
        }
    }
    if let Some(mut lingering) = lingering {
        lingering.wait(within(30)).unwrap();
    }
    assert!(!dir.exists());
}

#[test]
fn a_transition_that_waits_for_a_holder_that_provisions_nothing_finds_no_sandbox() {
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path().join("sandbox");
    let held = SandboxStore::lock_for_provision(&dir, &files(), None).unwrap();
    let waiter = thread::spawn({
        let dir = dir.clone();
        move || transition(&dir, within(30))
    });
    thread::sleep(Duration::from_millis(100));
    // The provision fails, and its release removes the lock file that the waiter waits for: the
    // waiter takes the lock of a new one in the directory, which holds no sandbox.
    drop(held);
    assert!(!waiter.join().unwrap().unwrap());
    assert!(entries_of(&dir).is_empty());
}

#[test]
fn transitions_in_other_processes_that_wait_through_a_removal_find_the_sandbox_gone() {
    let (_directory, dir) = sandbox();
    let locked = SandboxStore::lock_existing(&dir, &files(), None).unwrap();
    let mut transitions: Vec<_> = (0..2)
        .map(|_| Transition::start(&dir, "existing"))
        .collect();
    thread::sleep(Duration::from_millis(100));
    assert!(transitions.iter_mut().all(Transition::runs));
    let lingering = locked.remove().unwrap();
    let outcomes: Vec<_> = transitions.into_iter().map(Transition::outcome).collect();
    // As with threads, a transition that started late may find no sandbox rather than the
    // directory gone, whether the directory lingers or not.
    for outcome in &outcomes {
        assert!(
            ["gone", "unprovisioned"].contains(&outcome.as_str()),
            "{outcome}"
        );
    }
    if let Some(mut lingering) = lingering {
        lingering.wait(within(30)).unwrap();
    }
    assert!(!dir.exists());
}

#[test]
fn provisions_in_other_processes_that_race_a_removal_provision_the_sandbox_again() {
    // A provision that waits for the removal's lock creates the directory again once the removal
    // completes.
    let (_directory, dir) = sandbox();
    let locked = SandboxStore::lock_existing(&dir, &files(), None).unwrap();
    let mut provision = Transition::start(&dir, "provision");
    thread::sleep(Duration::from_millis(100));
    assert!(provision.runs());
    // A lingering removal reports the provision, or removes the directory that the provision's
    // handle of the removed lock file kept, as in the threads' case.
    if let Some(mut lingering) = locked.remove().unwrap() {
        assert!(matches!(
            lingering.wait(within(30)),
            Ok(()) | Err(RemovalError::ProvisionedAgain)
        ));
    }
    assert_eq!(provision.outcome(), "unprovisioned");
    assert_eq!(entries_of(&dir), sorted(&[CONFIG, LOCK_NAME]));
    // One that takes the lock that the removal released while lingering lock files keep the
    // directory provisions it again, which the removal reports.
    let mut lingering = remove_lingering(&dir);
    assert_eq!(
        Transition::start(&dir, "provision").outcome(),
        "unprovisioned"
    );
    fs::remove_file(dir.join(RENAMED)).unwrap();
    assert!(matches!(
        lingering.wait(within(10)),
        Err(RemovalError::ProvisionedAgain)
    ));
    assert_eq!(entries_of(&dir), sorted(&[CONFIG, LOCK_NAME]));
}

#[test]
fn refused_removals_keep_the_directory_for_transitions_in_other_processes() {
    // A removal that is refused while the store records a VM removes nothing, and the waiting
    // transition finds the sandbox.
    let (_directory, dir) = sandbox();
    fs::write(dir.join(RUNTIME_NAME), b"{}").unwrap();
    let locked = SandboxStore::lock_existing(&dir, &files(), None).unwrap();
    let mut transition = Transition::start(&dir, "existing");
    thread::sleep(Duration::from_millis(100));
    assert!(transition.runs());
    assert!(matches!(locked.remove(), Err(RemovalError::Launched)));
    assert_eq!(transition.outcome(), "provisioned");
    // One that is refused for foreign entries removed the sandbox's files: the waiting transition
    // finds no sandbox, and the directory keeps the foreign entries alone.
    fs::remove_file(dir.join(RUNTIME_NAME)).unwrap();
    fs::write(dir.join("notes.txt"), b"kept").unwrap();
    let locked = SandboxStore::lock_existing(&dir, &files(), None).unwrap();
    let mut transition = Transition::start(&dir, "existing");
    thread::sleep(Duration::from_millis(100));
    assert!(transition.runs());
    let Err(RemovalError::Foreign(names)) = locked.remove() else {
        panic!("foreign entries were not refused");
    };
    assert_eq!(names, ["notes.txt"]);
    assert_eq!(transition.outcome(), "unprovisioned");
    assert_eq!(entries_of(&dir), ["notes.txt"]);
}

/// Tests of lock files and directories that a Windows file system without POSIX deletion keeps
/// under their name until their last handle closes, which NTFS does for one deleted without POSIX
/// semantics.
#[cfg(windows)]
#[allow(unsafe_code)]
mod windows {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FileDispositionInfo, SetFileInformationByHandle,
    };

    use super::*;

    /// Deletes the file or empty directory `path` as a file system without POSIX deletion deletes
    /// one that other handles hold: its name stays, and opening it, or creating a file in the
    /// directory, fails, until the last handle closes, of which the returned one is the last
    /// unless others hold it.
    fn delete_pending(path: &Path) -> File {
        let file = fs::OpenOptions::new()
            .access_mode(DELETE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .unwrap();
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: the handle is open, and the information is a FILE_DISPOSITION_INFO of its size.
        let set = unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle() as HANDLE,
                FileDispositionInfo,
                (&raw const disposition).cast(),
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        };
        assert_ne!(set, 0, "{}", io::Error::last_os_error());
        file
    }

    #[test]
    fn locks_wait_for_a_removed_lock_file_that_a_handle_keeps() {
        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path().join("sandbox");
        SandboxStore::create(&dir).unwrap();
        fs::write(dir.join(LOCK_NAME), b"").unwrap();
        let pending = delete_pending(&dir.join(LOCK_NAME));
        for error in [
            SandboxStore::lock_for_provision(&dir, &files(), Some(Instant::now())).unwrap_err(),
            SandboxStore::lock_existing(&dir, &files(), Some(Instant::now())).unwrap_err(),
        ] {
            assert_eq!(error.kind(), ErrorKind::TimedOut, "{error}");
        }
        let provision = thread::spawn({
            let dir = dir.clone();
            move || SandboxStore::lock_for_provision(&dir, &files(), within(30)).map(drop)
        });
        thread::sleep(Duration::from_millis(100));
        drop(pending);
        provision.join().unwrap().unwrap();
        assert!(entries_of(&dir).is_empty());
    }

    #[test]
    fn provisions_wait_for_a_removed_directory_that_a_handle_keeps() {
        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path().join("sandbox");
        // A removal deleted the directory before the provision created it.
        SandboxStore::create(&dir).unwrap();
        let pending = delete_pending(&dir);
        let error =
            SandboxStore::lock_for_provision(&dir, &files(), Some(Instant::now())).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TimedOut, "{error}");
        let provision = thread::spawn({
            let dir = dir.clone();
            move || SandboxStore::lock_for_provision(&dir, &files(), within(30)).map(drop)
        });
        thread::sleep(Duration::from_millis(100));
        assert!(!provision.is_finished());
        drop(pending);
        provision.join().unwrap().unwrap();
        assert!(entries_of(&dir).is_empty());
        // A removal deletes the directory after the provision created it, before the provision
        // opens its lock file in it.
        let mut attempts = 0;
        let locked = SandboxStore::provision_lock(
            &dir,
            &files(),
            within(30),
            |dir| {
                attempts += 1;
                let store = SandboxStore::create(dir)?;
                if attempts == 1 {
                    let pending = delete_pending(dir);
                    thread::spawn(move || {
                        thread::sleep(Duration::from_millis(100));
                        drop(pending);
                    });
                }
                Ok(store)
            },
            SandboxStore::take_lifecycle_lock,
        )
        .unwrap();
        assert!(attempts > 1, "{attempts}");
        drop(locked);
        assert!(entries_of(&dir).is_empty());
    }

    #[test]
    fn a_removal_takes_a_binding_whose_deletion_is_pending_for_removed_and_waits_until_it_goes() {
        let (_directory, dir) = sandbox();
        fs::write(dir.join(CLONE_BINDING_NAME), b"{}").unwrap();
        fs::write(dir.join(LOG_NAME), b"log").unwrap();
        // A release removed the binding while a reader held it.
        let pending = delete_pending(&dir.join(CLONE_BINDING_NAME));
        let mut lingering = remove(&dir)
            .unwrap()
            .expect("the binding whose deletion is pending keeps the directory");
        // Everything else went, and the binding lingers, as a removed lock file does, rather than
        // being refused as an entry that is not the store's.
        assert_eq!(entries_of(&dir), [CLONE_BINDING_NAME]);
        let error = lingering.wait(Some(Instant::now())).unwrap_err();
        assert!(matches!(error, RemovalError::TimedOut(_)), "{error}");
        drop(pending);
        lingering.wait(within(10)).unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn a_removal_takes_the_clone_files_whose_deletion_is_pending_for_removed() {
        let (_directory, dir) = sandbox();
        fs::write(dir.join(CLONE_BINDING_NAME), b"{}").unwrap();
        for name in CLONE_FILE_NAMES {
            fs::write(dir.join(name), b"").unwrap();
        }
        // The removal of what a clone kept of its launch leaves the deletion of each file that a
        // reader holds pending, which the removal's own removal of them then finds.
        let pending: Vec<File> = CLONE_FILE_NAMES
            .into_iter()
            .map(|name| delete_pending(&dir.join(name)))
            .collect();
        let mut lingering = remove(&dir)
            .unwrap()
            .expect("the files whose deletion is pending keep the directory");
        // Everything else went, the binding included, and the clone's files linger, as a removed
        // lock file does, rather than being refused as entries that are not the store's.
        assert_eq!(entries_of(&dir), sorted(&CLONE_FILE_NAMES));
        let error = lingering.wait(Some(Instant::now())).unwrap_err();
        assert!(matches!(error, RemovalError::TimedOut(_)), "{error}");
        drop(pending);
        lingering.wait(within(10)).unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn a_retried_removal_of_the_clone_files_takes_those_whose_deletion_is_pending_for_removed() {
        let (_directory, dir) = sandbox();
        for name in CLONE_FILE_NAMES {
            fs::write(dir.join(name), b"").unwrap();
        }
        // A release that failed once it removed the clone's files, which readers hold, retries
        // their removal, which finds each one's deletion pending.
        let pending: Vec<File> = CLONE_FILE_NAMES
            .into_iter()
            .map(|name| delete_pending(&dir.join(name)))
            .collect();
        let locked = SandboxStore::lock_existing(&dir, &files(), None).unwrap();
        locked
            .remove_clone_files(&locked.open_dir().unwrap())
            .unwrap();
        drop(locked);
        drop(pending);
        let names = entries_of(&dir);
        assert!(
            names
                .iter()
                .all(|name| !CLONE_FILE_NAMES.contains(&name.as_str())),
            "{names:?}"
        );
    }

    #[test]
    fn locks_refuse_a_lock_file_that_refuses_access_for_good() {
        // A read-only lock file, and a directory at its path, refuse an open for writing as a
        // removed lock file does, but they open without access rights, so the locks fail at once,
        // also without a deadline.
        for read_only in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let dir = directory.path().join("sandbox");
            SandboxStore::create(&dir).unwrap();
            let lock = dir.join(LOCK_NAME);
            if read_only {
                fs::write(&lock, b"").unwrap();
                set_read_only(&lock, true);
            } else {
                fs::create_dir(&lock).unwrap();
            }
            let (sender, receiver) = mpsc::channel();
            thread::spawn({
                let dir = dir.clone();
                move || {
                    let provision = SandboxStore::lock_for_provision(&dir, &files(), None);
                    let existing = SandboxStore::lock_existing(&dir, &files(), None);
                    sender
                        .send([provision.map(drop), existing.map(drop)])
                        .unwrap();
                }
            });
            let taken = receiver
                .recv_timeout(Duration::from_secs(10))
                .expect("a lock waited for a lock file that refuses access for good");
            for result in taken {
                let error = result.unwrap_err();
                assert!(denied(&error), "{error}");
            }
            if read_only {
                set_read_only(&lock, false);
            }
        }
    }

    /// Sets or clears the read-only attribute of the file `path`.
    fn set_read_only(path: &Path, read_only: bool) {
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_readonly(read_only);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn a_transition_whose_lock_file_lingers_waits_until_it_goes() {
        let (_directory, dir) = sandbox();
        let held = SandboxStore::lock_existing(&dir, &files(), None).unwrap();
        let (ready, waiting) = mpsc::channel();
        let waiter = thread::spawn({
            let dir = dir.clone();
            move || {
                ready.send(()).unwrap();
                transition(&dir, within(30))
            }
        });
        waiting.recv().unwrap();
        thread::sleep(Duration::from_millis(100));
        // The holder removes the sandbox, on a file system that keeps the lock file while a
        // handle holds it. The waiter takes its lock, finds it removed, and cannot open the lock
        // file at the path again until it goes.
        fs::remove_file(dir.join(CONFIG)).unwrap();
        let pending = delete_pending(&dir.join(LOCK_NAME));
        drop(held);
        thread::sleep(Duration::from_millis(100));
        assert!(!waiter.is_finished());
        drop(pending);
        assert!(!waiter.join().unwrap().unwrap());
        assert!(entries_of(&dir).is_empty());
    }

    #[test]
    fn transitions_in_other_processes_wait_for_a_lingering_lock_file() {
        let (_directory, dir) = sandbox();
        fs::remove_file(dir.join(CONFIG)).unwrap();
        for (lock, left) in [
            ("existing", &[][..]),
            ("provision", &[CONFIG, LOCK_NAME][..]),
        ] {
            fs::write(dir.join(LOCK_NAME), b"").unwrap();
            let pending = delete_pending(&dir.join(LOCK_NAME));
            let mut transition = Transition::start(&dir, lock);
            thread::sleep(Duration::from_millis(100));
            assert!(transition.runs(), "{lock}");
            drop(pending);
            assert_eq!(transition.outcome(), "unprovisioned", "{lock}");
            assert_eq!(entries_of(&dir), sorted(left), "{lock}");
        }
    }
}
