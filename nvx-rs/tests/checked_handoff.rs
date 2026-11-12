//! The waits and the cleanup of a launched VMM, through the public API of `nvx::host::supervisor`,
//! as an orchestrator uses them: a wait returns and keeps the VMM's actual exit status, and the
//! strict cleanup of a VMM that runs ends it and confirms its exit.
//!
//! The VMM is this test executable, run again as a fixture. It reads the capability that
//! `nvx::host::supervisor::spawn` passes on its standard input, as OpenVMM does, reports that it
//! started, and runs until a file named `exit` appears in its working directory, or, as the exit
//! status fixture, until a file named `go` appears, when it exits with the status that the test
//! wrote.
#![cfg(all(feature = "host", any(target_os = "linux", windows)))]

use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use nvx::host::supervisor::{self, Launched, VmmCommand};
use nvx::protocol::CAPABILITY_LEN;

/// File whose presence in its working directory marks a VMM fixture. Without it, the fixture
/// returns at once, as it does when `cargo test -- --ignored` runs it.
const VMM_MARKER: &str = "vmm-fixture";
/// File whose appearance in its working directory lets the exit status fixture exit.
const GO_MARKER: &str = "go";
/// File that a VMM fixture writes once it has started.
const STARTED_REPORT: &str = "started";
/// File whose appearance in its working directory ends a VMM fixture.
const EXIT_MARKER: &str = "exit";
/// The capability of the VMMs of these tests.
const CAPABILITY: [u8; CAPABILITY_LEN] = [0x5a; CAPABILITY_LEN];
/// Longest wait for a fixture to answer.
const TIMEOUT: Duration = Duration::from_secs(60);
/// Longest life of a VMM fixture that nothing ends, so that a failed test leaves no process.
const VMM_LIFETIME: Duration = Duration::from_secs(180);

/// The VMM fixture: reads its capability as OpenVMM does, reports that it started, and runs until
/// [`EXIT_MARKER`] appears.
#[test]
#[ignore = "fixture that the cleanup test runs as a VMM"]
fn vmm_fixture() {
    let directory = env::current_dir().unwrap();
    if !directory.join(VMM_MARKER).exists() {
        return;
    }
    let mut capability = [0; CAPABILITY_LEN];
    io::stdin().read_exact(&mut capability).unwrap();
    assert_eq!(capability, CAPABILITY);
    write_whole(&directory.join(STARTED_REPORT), "");
    wait_for(&directory.join(EXIT_MARKER), VMM_LIFETIME);
}

/// Writes `text` to the file at `path`, which appears whole.
fn write_whole(path: &Path, text: &str) {
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, text).unwrap();
    fs::rename(&temporary, path).unwrap();
}

/// Waits until the file at `path` exists, for at most `timeout`, and returns whether it does.
fn wait_for(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
    true
}

/// Returns the arguments that run only the fixture `name` of this test executable.
fn fixture_arguments(name: &str) -> Vec<OsString> {
    [
        "--exact",
        name,
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]
    .map(OsString::from)
    .to_vec()
}

/// A VMM fixture that exits with the requested status once the caller lets it go.
#[test]
#[ignore = "fixture that the exit-status test runs as a VMM"]
fn exit_status_fixture() {
    let directory = env::current_dir().unwrap();
    if !directory.join(VMM_MARKER).exists() {
        return;
    }
    let mut capability = [0; CAPABILITY_LEN];
    io::stdin().read_exact(&mut capability).unwrap();
    assert_eq!(capability, CAPABILITY);
    let deadline = Instant::now() + VMM_LIFETIME;
    while !directory.join(GO_MARKER).exists() {
        if directory.join(EXIT_MARKER).exists() || Instant::now() >= deadline {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    let status = fs::read_to_string(directory.join("exit-status")).unwrap();
    std::process::exit(status.parse().unwrap());
}

#[test]
fn launched_waits_return_and_retain_the_actual_exit_status() {
    for status in [0, 17] {
        let mut vmm = VmmDirectory::new();
        fs::write(vmm.path().join("exit-status"), status.to_string()).unwrap();
        let command = VmmCommand::new(
            env::current_exe().unwrap(),
            fixture_arguments("exit_status_fixture"),
            vmm.path(),
        );
        let log = File::create(vmm.path().join("vmm.log")).unwrap();
        vmm.launched = Some(supervisor::spawn(&command, &CAPABILITY, log).unwrap());
        assert_eq!(
            vmm.launched
                .as_mut()
                .unwrap()
                .wait_timeout(Duration::ZERO)
                .unwrap(),
            None
        );
        File::create(vmm.path().join(GO_MARKER)).unwrap();
        let launched = vmm.launched.as_mut().unwrap();
        assert_eq!(launched.wait_timeout(TIMEOUT).unwrap(), Some(status));
        assert_eq!(launched.wait_timeout(Duration::ZERO).unwrap(), Some(status));
        vmm.launched
            .take()
            .unwrap()
            .kill_and_confirm_exit()
            .unwrap();
    }
}

#[test]
fn strict_cleanup_terminates_and_confirms_a_running_child() {
    let mut vmm = VmmDirectory::new();
    let command = VmmCommand::new(
        env::current_exe().unwrap(),
        fixture_arguments("vmm_fixture"),
        vmm.path(),
    );
    let log = File::create(vmm.path().join("vmm.log")).unwrap();
    vmm.launched = Some(supervisor::spawn_vm(&command, &CAPABILITY, log).unwrap());
    vmm.report(STARTED_REPORT);
    let process = vmm.launched.as_ref().unwrap().process().unwrap();
    assert!(!process.has_exited().unwrap());
    vmm.launched
        .take()
        .unwrap()
        .kill_and_confirm_exit()
        .unwrap();
    assert!(process.has_exited().unwrap());
}

/// The working directory of a VMM fixture. Dropping it ends the fixture, and the launched VMM
/// that it holds, so that a failed test leaves no process behind.
struct VmmDirectory {
    /// The launched VMM, once the test spawned it.
    launched: Option<Launched>,
    /// The directory, which is removed after the VMM exits.
    directory: tempfile::TempDir,
}

impl VmmDirectory {
    /// Creates a directory that marks a VMM fixture.
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        File::create(directory.path().join(VMM_MARKER)).unwrap();
        Self {
            launched: None,
            directory,
        }
    }

    /// Returns the path of the directory.
    fn path(&self) -> &Path {
        self.directory.path()
    }

    /// Waits for the VMM's report `name`, and returns it.
    fn report(&self, name: &str) -> String {
        let path = self.path().join(name);
        assert!(wait_for(&path, TIMEOUT), "the VMM did not report {name}");
        fs::read_to_string(path).unwrap()
    }
}

impl Drop for VmmDirectory {
    fn drop(&mut self) {
        let _ = File::create(self.path().join(EXIT_MARKER));
        if let Some(launched) = self.launched.take() {
            if let Ok(process) = launched.process() {
                let _ = process.wait(Instant::now() + TIMEOUT);
            }
            launched.kill_and_wait();
        }
    }
}
