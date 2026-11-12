//! Verified process references and event-driven exit waits, through the public API of
//! `nvx::host::process`, as an orchestrator uses them.
//!
//! The processes are this test executable, run again as one of two fixtures, which inherit
//! nothing of the test: a process fixture runs until a file named `exit` appears in its working
//! directory, and a launcher fixture starts a process fixture, prints the process's reference as
//! an orchestrator would record it, and runs until its standard input closes.
#![cfg(all(feature = "host", any(target_os = "linux", windows)))]

use std::env;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use nvx::host::platform;
use nvx::host::process::{BootId, Opened, Process, ProcessRef, WaitMethod};

/// File whose presence in its working directory marks a process fixture. Without it, the fixture
/// returns at once, as it does when `cargo test -- --ignored` runs it.
const PROCESS_MARKER: &str = "process-fixture";
/// File whose appearance in its working directory ends a process fixture.
const EXIT_MARKER: &str = "exit";
/// Environment variable that marks a launcher fixture, and names the working directory of its
/// process.
const LAUNCHER_DIR: &str = "NVX_PROCESS_TEST_DIR";
/// Marker before the reference that a launcher fixture prints. Libtest's own output, the name of
/// the fixture, precedes it on its line.
const REFERENCE_PREFIX: &str = "process-reference ";
/// Longest wait for a process that is expected to exit, or for a fixture to answer.
const TIMEOUT: Duration = Duration::from_secs(60);
/// Longest life of a process fixture that nothing ends, so that a failed test leaves no process.
const PROCESS_LIFETIME: Duration = Duration::from_secs(180);
/// A wait that a running process outlasts.
const SHORT_WAIT: Duration = Duration::from_millis(100);

/// Serializes the tests that spawn processes.
static SPAWNS: Mutex<()> = Mutex::new(());

/// Takes [`SPAWNS`].
fn serialized() -> MutexGuard<'static, ()> {
    SPAWNS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The process fixture: runs until a file named [`EXIT_MARKER`] appears in its working directory.
#[test]
#[ignore = "fixture that the process tests run"]
fn process_fixture() {
    let directory = env::current_dir().unwrap();
    if !directory.join(PROCESS_MARKER).exists() {
        return;
    }
    let started = Instant::now();
    while !directory.join(EXIT_MARKER).exists() && started.elapsed() < PROCESS_LIFETIME {
        thread::sleep(Duration::from_millis(10));
    }
}

/// The launcher fixture: when the test writes a line to its standard input, starts a process
/// fixture in the directory that [`LAUNCHER_DIR`] names, prints its reference, releases it, and
/// runs until its standard input closes.
#[test]
#[ignore = "fixture that the process tests run as a launcher"]
fn launcher_fixture() {
    let Some(directory) = env::var_os(LAUNCHER_DIR) else {
        return;
    };
    let mut input = io::stdin().lock();
    input.read_line(&mut String::new()).unwrap();
    let mut child = spawn_process(Path::new(&directory));
    println!(
        "{REFERENCE_PREFIX}{}",
        serde_json::to_string(&reference_of(&child)).unwrap()
    );
    // The launcher releases its process, which it reaps only if it exits first.
    thread::spawn(move || child.wait());
    io::copy(&mut input, &mut io::sink()).unwrap();
}

/// Returns the arguments that run only the fixture `name` of this test executable.
///
/// The fixture runs on libtest's main thread, whatever the host's processors or the inherited
/// `RUST_TEST_THREADS`, so libtest always prints its name before the fixture's own output.
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

/// Starts a process fixture in `directory`, which it marks, without standard streams.
fn spawn_process(directory: &Path) -> Child {
    File::create(directory.join(PROCESS_MARKER)).unwrap();
    Command::new(env::current_exe().unwrap())
        .args(fixture_arguments("process_fixture"))
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// Returns the reference of `child`, which has not been reaped, so that its ID still names it.
fn reference_of(child: &Child) -> ProcessRef {
    let pid = child.id();
    ProcessRef {
        pid,
        start_time: platform::process_start_time(pid).unwrap().unwrap(),
        boot: BootId::current().unwrap(),
    }
}

/// The working directory of a process fixture. Dropping it ends the fixture, or a process that
/// ignores the exit marker, and waits for it to exit, so that a failed test leaves no process
/// behind.
struct ProcessDirectory {
    /// The reference of the process, once the test knows it.
    reference: Option<ProcessRef>,
    /// The directory, which is removed after the process exits.
    directory: tempfile::TempDir,
}

impl ProcessDirectory {
    /// Creates an empty directory.
    fn new() -> Self {
        Self {
            reference: None,
            directory: tempfile::tempdir().unwrap(),
        }
    }

    /// Returns the path of the directory.
    fn path(&self) -> &Path {
        self.directory.path()
    }

    /// Records the reference of the process that runs in the directory.
    fn record(&mut self, reference: &ProcessRef) {
        self.reference = Some(reference.clone());
    }

    /// Asks the process to exit.
    fn end(&self) {
        File::create(self.path().join(EXIT_MARKER)).unwrap();
    }
}

impl Drop for ProcessDirectory {
    fn drop(&mut self) {
        let _ = File::create(self.path().join(EXIT_MARKER));
        if let Some(reference) = &self.reference
            && let Ok(Opened::Running(process)) = Process::open(reference)
            && !process
                .wait(Instant::now() + TIMEOUT)
                .is_ok_and(|exited| exited.is_some())
        {
            let _ = process.kill();
            let _ = process.wait(Instant::now() + TIMEOUT);
        }
    }
}

/// A child of the test, which is ended and reaped when dropped.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A launcher fixture, which the test drives through its standard streams.
struct Launcher {
    /// The fixture's process.
    child: Child,
    /// Its standard input, which the test closes to end it.
    input: Option<ChildStdin>,
}

impl Launcher {
    /// Starts a launcher whose process runs in `directory`.
    fn start(directory: &Path) -> Self {
        let mut child = Command::new(env::current_exe().unwrap())
            .args(fixture_arguments("launcher_fixture"))
            .env(LAUNCHER_DIR, directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        Self { child, input }
    }

    /// Asks the fixture to start its process, and returns the reference that it prints.
    fn launch_reference(&mut self) -> ProcessRef {
        let input = self.input.as_mut().unwrap();
        input.write_all(b"launch\n").unwrap();
        input.flush().unwrap();
        let output = self.child.stdout.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut lines = BufReader::new(output).lines();
            let reference = lines.by_ref().find_map(|line| {
                let line = line.ok()?;
                Some(line.split_once(REFERENCE_PREFIX)?.1.to_owned())
            });
            let _ = sender.send(reference);
            // Read the rest, so that the fixture can still write its own output.
            lines.for_each(drop);
        });
        let reference = receiver
            .recv_timeout(TIMEOUT)
            .expect("the fixture printed no reference in time")
            .expect("the fixture exited before it printed a reference");
        serde_json::from_str(&reference).unwrap()
    }

    /// Waits for the fixture to exit, and returns its status.
    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "the fixture did not exit");
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Closes the fixture's input, which ends it, and waits for it to exit.
    fn finish(&mut self) {
        drop(self.input.take());
        assert!(self.wait().success());
    }
}

impl Drop for Launcher {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Opens the process that `reference` names, which must still run.
fn open_running(reference: &ProcessRef) -> Process {
    let Opened::Running(process) = Process::open(reference).unwrap() else {
        panic!("{reference:?} does not run");
    };
    process
}

/// Checks that the process that `process` holds has exited, and that no process matches its
/// reference.
fn assert_ended(process: &Process) {
    assert!(process.has_exited().unwrap());
    assert!(matches!(
        Process::open(process.reference()).unwrap(),
        Opened::Exited(_)
    ));
}

#[test]
fn processes_can_be_shared_between_threads() {
    /// Compiles only for types that threads can share.
    fn shared<T: Send + Sync>() {}
    shared::<Process>();
    shared::<ProcessRef>();
}

#[test]
fn a_child_that_exits_wakes_its_waiter() {
    let _spawns = serialized();
    let mut directory = ProcessDirectory::new();
    let mut child = Reaped(spawn_process(directory.path()));
    let process = open_running(&reference_of(&child.0));
    directory.record(process.reference());
    assert_eq!(process.reference().pid, child.0.id());
    assert_eq!(process.reference().boot, BootId::current().unwrap());
    assert_eq!(process.wait_method(), WaitMethod::Event);
    assert!(!process.has_exited().unwrap());
    assert!(process.wait(Instant::now() + SHORT_WAIT).unwrap().is_none());
    // The reference finds the same process through a handle of its own.
    let found = open_running(process.reference());
    assert_eq!(found.reference(), process.reference());

    let started = Instant::now();
    directory.end();
    assert!(process.wait(started + TIMEOUT).unwrap().is_some());
    assert!(found.wait(started + TIMEOUT).unwrap().is_some());
    assert!(started.elapsed() < TIMEOUT);
    assert_ended(&process);
    // Ending a process that exited does nothing.
    process.kill().unwrap();
    assert!(child.0.wait().unwrap().success());
}

#[test]
fn a_process_that_is_not_a_child_wakes_its_waiters() {
    let _spawns = serialized();
    let mut directory = ProcessDirectory::new();
    let mut launcher = Launcher::start(directory.path());
    let reference = launcher.launch_reference();
    directory.record(&reference);
    // The launcher exits, so its process is no child of this process.
    launcher.finish();
    let process = open_running(&reference);
    assert_eq!(process.wait_method(), WaitMethod::Event);
    assert!(process.wait(Instant::now() + SHORT_WAIT).unwrap().is_none());

    let started = Instant::now();
    let waiters: Vec<bool> = thread::scope(|scope| {
        let waiters: Vec<_> = (0..2)
            .map(|_| scope.spawn(|| process.wait(started + TIMEOUT).unwrap().is_some()))
            .collect();
        directory.end();
        waiters
            .into_iter()
            .map(|waiter| waiter.join().unwrap())
            .collect()
    });
    assert_eq!(waiters, [true, true]);
    assert!(started.elapsed() < TIMEOUT);
    assert_ended(&process);
}

#[test]
fn a_reused_process_id_is_never_signaled() {
    let _spawns = serialized();
    let mut directory = ProcessDirectory::new();
    let mut child = Reaped(spawn_process(directory.path()));
    let process = open_running(&reference_of(&child.0));
    let reference = process.reference().clone();
    directory.record(&reference);
    let mut other_boot = *reference.boot.as_bytes();
    other_boot[15] ^= 1;
    // A process with the same ID and another start time is not the referenced one, and neither
    // is one of another boot: the referenced process exited, and its ID now names another.
    for impostor in [
        ProcessRef {
            start_time: reference.start_time + 1,
            ..reference.clone()
        },
        ProcessRef {
            start_time: reference.start_time - 1,
            ..reference.clone()
        },
        ProcessRef {
            boot: BootId::from_bytes(other_boot),
            ..reference.clone()
        },
    ] {
        assert!(
            matches!(Process::open(&impostor).unwrap(), Opened::Exited(_)),
            "{impostor:?} names a running process"
        );
    }

    // The process that has the ID still runs.
    assert!(!process.has_exited().unwrap());
    open_running(&reference);
    directory.end();
    assert!(process.wait(Instant::now() + TIMEOUT).unwrap().is_some());
    assert!(child.0.wait().unwrap().success());
}

#[test]
fn references_that_name_no_process_are_never_opened() {
    let _spawns = serialized();
    let boot = BootId::current().unwrap();
    let invalid = ProcessRef {
        pid: 0,
        start_time: 1,
        boot,
    };
    let error = Process::open(&invalid).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    // Linux assigns no process ID beyond `i32::MAX`.
    #[cfg(target_os = "linux")]
    {
        let invalid = ProcessRef {
            pid: u32::MAX,
            ..invalid
        };
        let error = Process::open(&invalid).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    // A process that exited and was reaped: no process has its ID, or another one has it.
    let directory = ProcessDirectory::new();
    let mut child = Reaped(spawn_process(directory.path()));
    let process = open_running(&reference_of(&child.0));
    directory.end();
    assert!(process.wait(Instant::now() + TIMEOUT).unwrap().is_some());
    assert!(child.0.wait().unwrap().success());
    assert!(matches!(
        Process::open(process.reference()).unwrap(),
        Opened::Exited(_)
    ));
    assert!(process.has_exited().unwrap());
}

#[cfg(windows)]
#[test]
fn a_process_that_this_user_may_not_query_has_an_unknown_state() {
    use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;

    let _spawns = serialized();
    let (mut child, reference) = restricted::spawn(0);
    if restricted::may_open(&child.0, PROCESS_QUERY_LIMITED_INFORMATION) {
        eprintln!("skipping: this user's privileges bypass the security descriptors of processes");
        return;
    }
    // Its identity cannot be checked, so it may still run.
    let error = Process::open(&reference).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "the process was ended"
    );
}

#[cfg(windows)]
#[test]
fn a_process_that_this_user_may_not_terminate_is_waited_for_but_never_signaled() {
    use windows_sys::Win32::System::Threading::{
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    };

    let _spawns = serialized();
    let (mut child, reference) =
        restricted::spawn(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE);
    if restricted::may_open(&child.0, PROCESS_TERMINATE) {
        eprintln!("skipping: this user's privileges bypass the security descriptors of processes");
        return;
    }
    let process = open_running(&reference);
    assert_eq!(process.wait_method(), WaitMethod::Event);
    let error = process.kill().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert!(!process.has_exited().unwrap());
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "the process was ended"
    );

    // Its creator ends it, which wakes the waiter, and ending a process that exited does nothing.
    child.0.kill().unwrap();
    assert!(process.wait(Instant::now() + TIMEOUT).unwrap().is_some());
    process.kill().unwrap();
    assert_ended(&process);
}

/// Processes whose security descriptor restricts what this user may do with them.
#[cfg(windows)]
mod restricted {
    use std::os::windows::io::AsRawHandle;
    use std::process::{Child, Command, Stdio};
    use std::{io, mem, ptr};

    use nvx::host::platform;
    use nvx::host::process::{BootId, ProcessRef};
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS};
    use windows_sys::Win32::Security::Authorization::{SE_KERNEL_OBJECT, SetSecurityInfo};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, AddAccessAllowedAce, CreateWellKnownSid,
        DACL_SECURITY_INFORMATION, InitializeAcl, PROTECTED_DACL_SECURITY_INFORMATION,
        SECURITY_MAX_SID_SIZE, WinWorldSid,
    };
    use windows_sys::Win32::System::Threading::OpenProcess;

    /// A child process, which is ended when dropped, through the handle with full access that
    /// its creator holds.
    pub(crate) struct Running(pub(crate) Child);

    impl Drop for Running {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Starts a process that runs for a minute unless it is ended, and whose security
    /// descriptor then grants everyone, this user included, only `access` to it. Returns it with
    /// its reference.
    pub(crate) fn spawn(access: u32) -> (Running, ProcessRef) {
        let child = Command::new("ping")
            .args(["-n", "60", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let child = Running(child);
        let pid = child.0.id();
        let reference = ProcessRef {
            pid,
            start_time: platform::process_start_time(pid).unwrap().unwrap(),
            boot: BootId::current().unwrap(),
        };
        restrict(&child.0, access);
        (child, reference)
    }

    /// Replaces the DACL of `child` with one that grants everyone `access`, or no access when
    /// `access` is 0.
    fn restrict(child: &Child, access: u32) {
        let mut sid = [0u8; SECURITY_MAX_SID_SIZE as usize];
        let mut sid_size = SECURITY_MAX_SID_SIZE;
        // SAFETY: the buffer holds `sid_size` bytes.
        let created = unsafe {
            CreateWellKnownSid(
                WinWorldSid,
                ptr::null_mut(),
                sid.as_mut_ptr().cast(),
                &mut sid_size,
            )
        };
        assert_ne!(created, 0, "{}", io::Error::last_os_error());
        // Room for the ACL header, one ACE, and its SID, in 4-byte units, which align the ACL.
        let size = mem::size_of::<ACL>()
            + mem::size_of::<ACCESS_ALLOWED_ACE>()
            + usize::try_from(sid_size).unwrap();
        let mut buffer = vec![0u32; size.div_ceil(4)];
        let acl: *mut ACL = buffer.as_mut_ptr().cast();
        let length = u32::try_from(buffer.len() * 4).unwrap();
        // SAFETY: the buffer holds `length` bytes, aligned for an ACL.
        let initialized = unsafe { InitializeAcl(acl, length, ACL_REVISION) };
        assert_ne!(initialized, 0, "{}", io::Error::last_os_error());
        if access != 0 {
            // SAFETY: the ACL has room for the ACE, and the SID is valid.
            let added =
                unsafe { AddAccessAllowedAce(acl, ACL_REVISION, access, sid.as_mut_ptr().cast()) };
            assert_ne!(added, 0, "{}", io::Error::last_os_error());
        }
        // SAFETY: the creator's handle has WRITE_DAC access, and the ACL is valid.
        let status = unsafe {
            SetSecurityInfo(
                child.as_raw_handle(),
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                acl,
                ptr::null(),
            )
        };
        assert_eq!(
            status,
            ERROR_SUCCESS,
            "cannot restrict a process: {}",
            io::Error::from_raw_os_error(status as i32)
        );
    }

    /// Returns whether this user may still open `child` with `access`, as a user may whose
    /// privileges bypass security descriptors.
    pub(crate) fn may_open(child: &Child, access: u32) -> bool {
        // SAFETY: OpenProcess has no memory-safety preconditions.
        let handle = unsafe { OpenProcess(access, 0, child.id()) };
        if handle.is_null() {
            return false;
        }
        // SAFETY: the handle is open, and nothing else owns it.
        unsafe { CloseHandle(handle) };
        true
    }
}
