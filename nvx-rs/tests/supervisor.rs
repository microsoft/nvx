//! Verified process references, event-driven exit waits, and the supervision of VMM processes,
//! through the public API of `nvx::host::process`, `nvx::host::supervisor`, and
//! `nvx::host::store`, as an orchestrator uses them.
//!
//! The processes are this test executable, run again as one of three fixtures. A VMM fixture
//! reads the capability that `nvx::host::supervisor::spawn` passes on its standard input, as
//! OpenVMM does, reports its descriptors on Linux, writes a line through each descriptor or handle
//! that its arguments name as inherited, and runs until a file named `exit` appears in its working
//! directory. A launcher fixture spawns a VMM fixture, prints the VMM's reference as an
//! orchestrator would record it, and runs until its standard input closes. An orchestrator fixture
//! launches a VMM fixture through the supervisor in a sandbox store, and exits without unwinding
//! after the launch step that the test names, as a crash would, or, once it has committed the
//! launch, runs until its standard input closes. A fixture that the test kills, or whose job
//! object the test closes, stands for an orchestrator that crashed and restarts within the same
//! boot: the test then finds the VMM again, as the restarted orchestrator would.
#![cfg(all(feature = "host", any(target_os = "linux", windows)))]

use std::convert::Infallible;
use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, PipeReader, PipeWriter, Read, Write};
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use nvx::ErrorKind;
use nvx::host::launch::MAX_TIMEOUT;
use nvx::host::process::{BootId, Process, ProcessRef, WaitMethod};
#[cfg(windows)]
use nvx::host::store::FORMAT;
use nvx::host::store::{
    ArtifactIdentity, IdentityPart, LaunchId, LaunchState, LockedStore, RUNTIME_NAME, Recovery,
    RuntimeIdentity, SandboxStore,
};
use nvx::host::supervisor::{self, Inherited, Launched, Reattached, StopOutcome, Vmm, VmmCommand};
use nvx::protocol::CAPABILITY_LEN;
use serde_json::json;

/// File whose presence in its working directory marks a VMM fixture. Without it, the fixture
/// returns at once, as it does when `cargo test -- --ignored` runs it.
const VMM_MARKER: &str = "vmm-fixture";
/// File whose appearance in its working directory ends a VMM fixture.
const EXIT_MARKER: &str = "exit";
/// File in which a VMM fixture reports its descriptors on Linux, a line for each: its number and
/// the object that it holds.
#[cfg(target_os = "linux")]
const DESCRIPTORS_REPORT: &str = "descriptors";
/// Prefix of an argument of a VMM fixture that names, in decimal, a descriptor or handle that the
/// fixture inherited, and through which it writes a line. Libtest takes the argument for a test
/// name that matches no test.
const INHERITED_PREFIX: &str = "inherited=";
/// Environment variable that marks a launcher fixture, and names the working directory of the VMM
/// of a launcher or orchestrator fixture.
const LAUNCHER_DIR: &str = "NVX_SUPERVISOR_TEST_VMM_DIR";
/// Environment variable that asks a launcher or orchestrator fixture to start its VMM outside its
/// job object.
const LAUNCHER_BREAKAWAY: &str = "NVX_SUPERVISOR_TEST_BREAKAWAY";
/// Environment variable that marks an orchestrator fixture, and names its sandbox store.
const ORCHESTRATOR_STORE: &str = "NVX_SUPERVISOR_TEST_STORE";
/// Environment variable that names the launch step after which an orchestrator fixture crashes:
/// `before-spawn`, `after-spawn`, or `after-identity`; or `committed`, after which it runs until
/// its standard input closes.
const ORCHESTRATOR_STEP: &str = "NVX_SUPERVISOR_TEST_STEP";
/// Environment variable that asks an orchestrator fixture to launch the fake OpenVMM, which
/// serves its control endpoint, instead of a VMM fixture.
#[cfg(feature = "testing")]
const ORCHESTRATOR_FAKE: &str = "NVX_SUPERVISOR_TEST_FAKE";
/// Marker before the reference that a launcher or orchestrator fixture prints. Libtest's own
/// output, the name of the fixture, precedes it on its line.
const REFERENCE_PREFIX: &str = "vmm-reference ";
/// The capability of every VMM of these tests.
const CAPABILITY: [u8; CAPABILITY_LEN] = [0x5a; CAPABILITY_LEN];
/// Longest wait for a process that is expected to exit, or for a fixture to answer.
const TIMEOUT: Duration = Duration::from_secs(60);
/// Longest life of a VMM fixture that nothing ends, so that a failed test leaves no process.
const VMM_LIFETIME: Duration = Duration::from_secs(180);
/// A wait that a running process outlasts.
const SHORT_WAIT: Duration = Duration::from_millis(100);

/// Serializes the tests that spawn processes. On Windows, a process that one test spawns while
/// another test's handles are inheritable for a spawn of its own, as the log of a launch is,
/// could hold them, and so the claim of that log, for a while.
static SPAWNS: Mutex<()> = Mutex::new(());

/// Takes [`SPAWNS`].
fn serialized() -> MutexGuard<'static, ()> {
    SPAWNS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The VMM fixture: reads its capability as OpenVMM does, reports its descriptors on Linux,
/// writes a line through each descriptor or handle that an argument names as inherited, then runs
/// until a file named [`EXIT_MARKER`] appears in its working directory.
#[test]
#[ignore = "fixture that the supervision tests run as a VMM"]
fn vmm_fixture() {
    let directory = env::current_dir().unwrap();
    if !directory.join(VMM_MARKER).exists() {
        return;
    }
    let mut capability = [0; CAPABILITY_LEN];
    io::stdin().read_exact(&mut capability).unwrap();
    assert_eq!(capability, CAPABILITY);
    #[cfg(target_os = "linux")]
    report_descriptors(&directory);
    for argument in env::args() {
        if let Some(number) = argument.strip_prefix(INHERITED_PREFIX) {
            // A VMM keeps what it inherits open.
            let mut inherited = ManuallyDrop::new(inherited_file(number.parse().unwrap()));
            writeln!(inherited, "inherited {number}").unwrap();
        }
    }
    let started = Instant::now();
    while !directory.join(EXIT_MARKER).exists() && started.elapsed() < VMM_LIFETIME {
        thread::sleep(Duration::from_millis(10));
    }
}

/// Writes the descriptors of this VMM fixture, but for the one that lists them, to the file
/// [`DESCRIPTORS_REPORT`] in `directory`, which appears whole.
#[cfg(target_os = "linux")]
fn report_descriptors(directory: &Path) {
    // The descriptor that lists the others holds this directory.
    let listing = PathBuf::from(format!("/proc/{}/fd", std::process::id()));
    let mut descriptors: Vec<(usize, PathBuf)> = fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.unwrap();
            let number = entry.file_name().to_str().unwrap().parse().unwrap();
            let object = fs::read_link(entry.path()).ok()?;
            (object != listing).then_some((number, object))
        })
        .collect();
    descriptors.sort();
    let report: String = descriptors
        .iter()
        .map(|(number, object)| format!("{number} {}\n", object.display()))
        .collect();
    let temporary = directory.join(format!("{DESCRIPTORS_REPORT}.tmp"));
    fs::write(&temporary, report).unwrap();
    fs::rename(&temporary, directory.join(DESCRIPTORS_REPORT)).unwrap();
}

/// Takes the descriptor `number`, which this VMM fixture inherited.
#[cfg(target_os = "linux")]
fn inherited_file(number: usize) -> File {
    use std::os::fd::FromRawFd;

    // SAFETY: the test named a descriptor that this process inherited under that number, and
    // that nothing else in this process owns.
    unsafe { File::from_raw_fd(i32::try_from(number).unwrap()) }
}

/// Takes the handle `number`, which this VMM fixture inherited.
#[cfg(windows)]
fn inherited_file(number: usize) -> File {
    use std::os::windows::io::{FromRawHandle, RawHandle};

    // SAFETY: the test named a handle that this process inherited under that value, and that
    // nothing else in this process owns.
    unsafe { File::from_raw_handle(number as RawHandle) }
}

/// The launcher fixture: when the test writes a line to its standard input, spawns a VMM fixture
/// in the directory that [`LAUNCHER_DIR`] names, prints its reference, releases it, and runs
/// until its standard input closes.
#[test]
#[ignore = "fixture that the supervision tests run as a launcher"]
fn launcher_fixture() {
    let Some(directory) = env::var_os(LAUNCHER_DIR) else {
        return;
    };
    let mut input = io::stdin().lock();
    input.read_line(&mut String::new()).unwrap();
    let breakaway = env::var_os(LAUNCHER_BREAKAWAY).is_some();
    let launched = spawn_vmm(Path::new(&directory), breakaway);
    print_reference(launched.process().unwrap().reference());
    launched.detach();
    io::copy(&mut input, &mut io::sink()).unwrap();
}

/// The orchestrator fixture: when the test writes a line to its standard input, launches a VMM
/// fixture, or the fake OpenVMM, through the supervisor in the store that [`ORCHESTRATOR_STORE`]
/// names, and stops after the step that [`ORCHESTRATOR_STEP`] names. It prints the reference of
/// the VMM once it has spawned it.
#[test]
#[ignore = "fixture that the supervision tests run as an orchestrator"]
fn orchestrator_fixture() {
    let Some(store) = env::var_os(ORCHESTRATOR_STORE) else {
        return;
    };
    let step = env::var(ORCHESTRATOR_STEP).unwrap();
    let directory = PathBuf::from(env::var_os(LAUNCHER_DIR).unwrap());
    let breakaway = env::var_os(LAUNCHER_BREAKAWAY).is_some();
    let mut input = io::stdin().lock();
    input.read_line(&mut String::new()).unwrap();
    let store = SandboxStore::open(store).unwrap();
    let mut locked = store.lock(None).unwrap();
    let command = |endpoint: &str| -> nvx::Result<VmmCommand<'static>> {
        #[cfg(feature = "testing")]
        if env::var_os(ORCHESTRATOR_FAKE).is_some() {
            return Ok(fake_openvmm(&directory, endpoint, &store));
        }
        let _ = endpoint;
        Ok(vmm_command(&directory, breakaway))
    };
    match step.as_str() {
        "before-spawn" => {
            let _ = supervisor::launch(
                &mut locked,
                &LaunchId::generate().unwrap(),
                &runtime(),
                &CAPABILITY,
                |_| crash(),
            );
            unreachable!("the orchestrator crashed before its spawn");
        }
        "after-spawn" => {
            // The steps of `supervisor::launch`, up to the spawn.
            let mut launch = locked
                .begin_launch(&LaunchId::generate().unwrap(), &runtime(), &CAPABILITY)
                .unwrap();
            let command = command(launch.endpoint()).unwrap();
            let log = launch.take_log().unwrap();
            let launched = supervisor::spawn(&command, &CAPABILITY, log).unwrap();
            print_reference(launched.process().unwrap().reference());
            crash();
        }
        "after-identity" => {
            let starting = supervisor::launch(
                &mut locked,
                &LaunchId::generate().unwrap(),
                &runtime(),
                &CAPABILITY,
                command,
            )
            .unwrap();
            print_reference(starting.process().reference());
            crash();
        }
        "committed" => {
            let starting = supervisor::launch(
                &mut locked,
                &LaunchId::generate().unwrap(),
                &runtime(),
                &CAPABILITY,
                command,
            )
            .unwrap();
            let vmm = starting.commit().unwrap();
            drop(locked);
            print_reference(vmm.record().process());
            io::copy(&mut input, &mut io::sink()).unwrap();
        }
        step => panic!("unknown launch step {step}"),
    }
}

/// Ends this process at once, once its output is flushed, without unwinding or running
/// destructors, as a crash would.
fn crash() -> ! {
    io::stdout().flush().unwrap();
    std::process::exit(0)
}

/// Prints `reference` as JSON, after [`REFERENCE_PREFIX`].
fn print_reference(reference: &ProcessRef) {
    println!(
        "{REFERENCE_PREFIX}{}",
        serde_json::to_string(reference).unwrap()
    );
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

/// Returns the identity `sha256:` and 64 copies of `digit`.
fn artifact(digit: char) -> ArtifactIdentity {
    ArtifactIdentity::sha256(&digit.to_string().repeat(64)).unwrap()
}

/// Returns the runtime identity of the launches of these tests.
fn runtime() -> RuntimeIdentity {
    RuntimeIdentity::new(artifact('1'), artifact('2'), artifact('3'))
}

/// Returns a sandbox store in a fresh temporary directory, which it returns too.
fn sandbox() -> (tempfile::TempDir, SandboxStore) {
    let directory = tempfile::tempdir().unwrap();
    let store = SandboxStore::create(directory.path().join("sandbox")).unwrap();
    (directory, store)
}

/// Returns the command of a VMM fixture in `directory`, which it marks, outside the caller's job
/// object if `breakaway`.
fn vmm_command(directory: &Path, breakaway: bool) -> VmmCommand<'static> {
    File::create(directory.join(VMM_MARKER)).unwrap();
    let mut command = VmmCommand::new(
        env::current_exe().unwrap(),
        fixture_arguments("vmm_fixture"),
        directory,
    );
    command.breakaway_from_job = breakaway;
    command
}

/// Returns the command of the fake OpenVMM in `directory`, which serves `endpoint` at once, and
/// writes its outcome report to that of `store`.
#[cfg(feature = "testing")]
fn fake_openvmm(directory: &Path, endpoint: &str, store: &SandboxStore) -> VmmCommand<'static> {
    use nvx::host::launch::LaunchSpec;
    use nvx::{Artifacts, Hypervisor};

    fs::write(directory.join("vmlinux"), b"kernel").unwrap();
    fs::write(directory.join("initramfs.cpio.gz"), b"initrd").unwrap();
    let artifacts = Artifacts::new(
        env!("CARGO_BIN_EXE_nvx-fake-openvmm"),
        directory.join("vmlinux"),
        directory.join("initramfs.cpio.gz"),
    );
    let mut spec = LaunchSpec::new(artifacts, Hypervisor::platform_default().unwrap());
    spec.kernel_parameters = "fake_boot_delay_ms=0".to_owned();
    let arguments = spec
        .openvmm_arguments(endpoint, &store.outcome_path())
        .unwrap();
    VmmCommand::new(&spec.openvmm, arguments, directory)
}

/// Spawns a VMM fixture in `directory` as OpenVMM is spawned, outside the caller's job object if
/// `breakaway`.
fn spawn_vmm(directory: &Path, breakaway: bool) -> Launched {
    let log = File::create(directory.join("vmm.log")).unwrap();
    supervisor::spawn(&vmm_command(directory, breakaway), &CAPABILITY, log).unwrap()
}

/// Launches a VMM fixture in `vmm` through the supervisor in `locked`, under [`runtime`], and
/// commits the launch, as an orchestrator does once the VMM serves its control endpoint.
fn launch_vmm(locked: &mut LockedStore, vmm: &mut VmmDirectory) -> Vmm {
    let starting = supervisor::launch(
        locked,
        &LaunchId::generate().unwrap(),
        &runtime(),
        &CAPABILITY,
        |endpoint| {
            assert!(!endpoint.is_empty());
            Ok(vmm_command(vmm.path(), false))
        },
    )
    .unwrap();
    vmm.record(starting.process().reference());
    // The launch recorded its VMM before it returned.
    assert_eq!(starting.journal().state(), LaunchState::Spawned);
    assert_eq!(
        starting.journal().process(),
        Some(starting.process().reference())
    );
    starting.commit().unwrap()
}

/// Reattaches until the result is determinate, as a caller retries an indeterminate reattach, and
/// returns it.
///
/// On Windows, a scanner that briefly opens the log of a launch can keep its claim held for a
/// moment.
fn reattach_until_determinate(locked: &mut LockedStore) -> Reattached {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match supervisor::reattach(locked, &runtime()).unwrap() {
            Reattached::Indeterminate(reason) if Instant::now() < deadline => {
                eprintln!("reattach is indeterminate: {reason}");
                thread::sleep(SHORT_WAIT);
            }
            reattached => return reattached,
        }
    }
}

/// Writes the runtime record of a VM whose VMM is `process` to `store`, as another orchestrator
/// would have committed it.
#[cfg(windows)]
fn write_record(store: &SandboxStore, process: &ProcessRef) {
    let record = json!({
        "format": FORMAT,
        "launch": "5d41402abc4b2a76b9719d911017c592",
        "process": process,
        "endpoint": r"\\.\pipe\openvmm-microvm-5d41402abc4b2a76b9719d911017c592",
        "runtime": runtime(),
    });
    fs::write(
        store.dir().join(RUNTIME_NAME),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
}

/// The working directory of a VMM fixture. Dropping it ends the fixture, or a VMM that ignores
/// the exit marker, as the fake OpenVMM does, and waits for it to exit, so that a failed test
/// leaves no process behind.
struct VmmDirectory {
    /// The reference of the VMM, once the test knows it.
    reference: Option<ProcessRef>,
    /// The directory, which is removed after the VMM exits.
    directory: tempfile::TempDir,
}

impl VmmDirectory {
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

    /// Records the reference of the VMM that runs in the directory.
    fn record(&mut self, reference: &ProcessRef) {
        self.reference = Some(reference.clone());
    }

    /// Asks the VMM to exit.
    fn end(&self) {
        File::create(self.path().join(EXIT_MARKER)).unwrap();
    }
}

impl Drop for VmmDirectory {
    fn drop(&mut self) {
        let _ = File::create(self.path().join(EXIT_MARKER));
        if let Some(reference) = &self.reference
            && let Ok(Some(process)) = Process::open(reference)
            && !process.wait(Instant::now() + TIMEOUT).unwrap_or(false)
        {
            let _ = process.kill();
            let _ = process.wait(Instant::now() + TIMEOUT);
        }
    }
}

/// A launcher or orchestrator fixture, which the test drives through its standard streams.
struct Launcher {
    /// The fixture's process.
    child: Child,
    /// Its standard input, which the test closes to end it.
    input: Option<ChildStdin>,
}

impl Launcher {
    /// Starts a launcher whose VMM runs in `directory`, outside the launcher's job object if
    /// `breakaway`.
    fn start(directory: &Path, breakaway: bool) -> Self {
        let mut command = Command::new(env::current_exe().unwrap());
        command
            .args(fixture_arguments("launcher_fixture"))
            .env(LAUNCHER_DIR, directory);
        if breakaway {
            command.env(LAUNCHER_BREAKAWAY, "1");
        }
        Self::spawn(command)
    }

    /// Returns the command of an orchestrator that launches a VMM in `directory` through `store`,
    /// and stops after `step`.
    fn orchestrator(store: &SandboxStore, directory: &Path, step: &str) -> Command {
        let mut command = Command::new(env::current_exe().unwrap());
        command
            .args(fixture_arguments("orchestrator_fixture"))
            .env(ORCHESTRATOR_STORE, store.dir())
            .env(ORCHESTRATOR_STEP, step)
            .env(LAUNCHER_DIR, directory);
        command
    }

    /// Starts an orchestrator that launches a VMM in `directory` through `store`, and stops after
    /// `step`.
    fn orchestrate(store: &SandboxStore, directory: &Path, step: &str) -> Self {
        Self::spawn(Self::orchestrator(store, directory, step))
    }

    /// Starts the fixture that `command` runs, with its standard input and output piped.
    fn spawn(mut command: Command) -> Self {
        command.stdin(Stdio::piped()).stdout(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let input = child.stdin.take();
        Self { child, input }
    }

    /// Asks the fixture to start its VMM, and returns the reference that it prints, as the
    /// fixture would record it: as JSON. Returns `None` if the fixture exits without printing
    /// one.
    fn try_launch(&mut self) -> Option<String> {
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
        receiver
            .recv_timeout(TIMEOUT)
            .expect("the fixture printed no reference in time")
    }

    /// Asks the fixture to start its VMM, and returns the reference that it prints, as the
    /// fixture would record it: as JSON.
    fn launch(&mut self) -> String {
        self.try_launch()
            .expect("the fixture exited before it printed a reference")
    }

    /// Asks the fixture to start its VMM, and returns the reference that it prints.
    fn launch_reference(&mut self) -> ProcessRef {
        serde_json::from_str(&self.launch()).unwrap()
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
    Process::open(reference)
        .unwrap()
        .unwrap_or_else(|| panic!("{reference:?} does not run"))
}

/// Checks that the process that `process` holds has exited, and that no process matches its
/// reference.
fn assert_ended(process: &Process) {
    assert!(process.has_exited().unwrap());
    assert!(Process::open(process.reference()).unwrap().is_none());
}

#[test]
fn processes_can_be_shared_between_threads() {
    /// Compiles only for types that threads can share.
    fn shared<T: Send + Sync>() {}
    shared::<Process>();
    shared::<ProcessRef>();
    shared::<Vmm>();
}

#[test]
fn a_child_that_exits_wakes_its_waiter() {
    let _spawns = serialized();
    let mut vmm = VmmDirectory::new();
    let launched = spawn_vmm(vmm.path(), false);
    let process = launched.process().unwrap();
    vmm.record(process.reference());
    assert_eq!(process.reference().pid, launched.id());
    assert_eq!(process.reference().boot, BootId::current().unwrap());
    assert_eq!(process.wait_method(), WaitMethod::Event);
    assert!(!process.has_exited().unwrap());
    assert!(!process.wait(Instant::now() + SHORT_WAIT).unwrap());
    // The reference finds the same process through a handle of its own.
    let found = open_running(process.reference());
    assert_eq!(found.reference(), process.reference());

    let started = Instant::now();
    vmm.end();
    assert!(process.wait(started + TIMEOUT).unwrap());
    assert!(found.wait(started + TIMEOUT).unwrap());
    assert!(started.elapsed() < TIMEOUT);
    assert_ended(&process);
    // Ending a process that exited does nothing.
    process.kill().unwrap();
    assert!(launched.kill_and_wait());
}

#[test]
fn a_process_that_is_not_a_child_wakes_its_waiters() {
    let _spawns = serialized();
    let mut vmm = VmmDirectory::new();
    let mut launcher = Launcher::start(vmm.path(), false);
    let reference = launcher.launch_reference();
    vmm.record(&reference);
    // The launcher exits, so its VMM is no child of this process.
    launcher.finish();
    let process = open_running(&reference);
    assert_eq!(process.wait_method(), WaitMethod::Event);
    assert!(!process.wait(Instant::now() + SHORT_WAIT).unwrap());

    let started = Instant::now();
    let waiters: Vec<bool> = thread::scope(|scope| {
        let waiters: Vec<_> = (0..2)
            .map(|_| scope.spawn(|| process.wait(started + TIMEOUT).unwrap()))
            .collect();
        vmm.end();
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
fn a_restarted_launcher_finds_its_vmm_again_and_ends_it() {
    let _spawns = serialized();
    let mut vmm = VmmDirectory::new();
    // On Windows, the launcher runs in a job object that terminates it when the job closes, and
    // its VMM breaks away from the job, as an orchestrator's VMMs do to survive it.
    #[cfg(windows)]
    let job = job::Job::new(true);
    let mut launcher = Launcher::start(vmm.path(), cfg!(windows));
    #[cfg(windows)]
    job.assign(&launcher.child);
    let recorded = launcher.launch();
    // The launcher crashes.
    #[cfg(windows)]
    drop(job);
    #[cfg(not(windows))]
    launcher.child.kill().unwrap();
    launcher.wait();

    // Its successor reads the recorded reference back, and finds the VMM, which survived.
    let reference: ProcessRef = serde_json::from_str(&recorded).unwrap();
    vmm.record(&reference);
    let process = open_running(&reference);
    assert_eq!(process.wait_method(), WaitMethod::Event);
    assert!(!process.wait(Instant::now() + SHORT_WAIT).unwrap());
    let started = Instant::now();
    process.kill().unwrap();
    assert!(process.wait(started + TIMEOUT).unwrap());
    assert_ended(&process);
}

#[test]
fn a_reused_process_id_is_never_signaled() {
    let _spawns = serialized();
    let mut vmm = VmmDirectory::new();
    let launched = spawn_vmm(vmm.path(), false);
    let process = launched.process().unwrap();
    let reference = process.reference().clone();
    vmm.record(&reference);
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
            Process::open(&impostor).unwrap().is_none(),
            "{impostor:?} names a running process"
        );
    }

    // The process that has the ID still runs.
    assert!(!process.has_exited().unwrap());
    open_running(&reference);
    vmm.end();
    assert!(process.wait(Instant::now() + TIMEOUT).unwrap());
    assert!(launched.kill_and_wait());
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
    let vmm = VmmDirectory::new();
    let launched = spawn_vmm(vmm.path(), false);
    let process = launched.process().unwrap();
    vmm.end();
    assert!(process.wait(Instant::now() + TIMEOUT).unwrap());
    assert!(launched.kill_and_wait());
    assert!(Process::open(process.reference()).unwrap().is_none());
    assert!(process.has_exited().unwrap());
}

#[test]
fn a_launch_that_crashes_before_its_spawn_is_ended_on_reattach() {
    let _spawns = serialized();
    let (_directory, store) = sandbox();
    let vmm = VmmDirectory::new();
    let mut orchestrator = Launcher::orchestrate(&store, vmm.path(), "before-spawn");
    assert_eq!(orchestrator.try_launch(), None);
    assert!(orchestrator.wait().success());
    assert_eq!(
        store.journal().unwrap().unwrap().state(),
        LaunchState::Prepared
    );
    assert!(!vmm.path().join(VMM_MARKER).exists());
    // The crash released the orchestrator's lock, and the claim of the log that it never passed
    // on.
    let mut locked = store.lock(Some(Instant::now() + TIMEOUT)).unwrap();
    match reattach_until_determinate(&mut locked) {
        Reattached::Exited {
            record: None,
            recovery: Recovery::Exited,
        } => {}
        other => panic!("unexpected reattach: {other:?}"),
    }
    assert!(store.journal().unwrap().is_none());
    assert!(store.read_capability().is_err());
}

#[test]
fn an_unrecorded_vmm_that_serves_no_endpoint_is_never_signaled() {
    let _spawns = serialized();
    let (_directory, store) = sandbox();
    let mut vmm = VmmDirectory::new();
    let mut orchestrator = Launcher::orchestrate(&store, vmm.path(), "after-spawn");
    let reference = orchestrator.launch_reference();
    vmm.record(&reference);
    assert!(orchestrator.wait().success());
    assert_eq!(
        store.journal().unwrap().unwrap().state(),
        LaunchState::Prepared
    );
    // The VMM, which is no child of this process, holds the claim of its launch, and serves no
    // control endpoint that would name it, so reattach can neither prove that it exited nor
    // verify it: it signals nothing, and keeps the journal.
    let process = open_running(&reference);
    let mut locked = store.lock(Some(Instant::now() + TIMEOUT)).unwrap();
    let Reattached::Indeterminate(reason) = supervisor::reattach(&mut locked, &runtime()).unwrap()
    else {
        panic!("reattach ended a launch whose VMM runs unverified");
    };
    assert!(
        reason.contains("no process serves its control endpoint yet"),
        "{reason}"
    );
    assert!(!process.has_exited().unwrap());
    assert!(store.journal().unwrap().is_some());
    // Once the VMM exits, the claim that it released proves it.
    vmm.end();
    assert!(process.wait(Instant::now() + TIMEOUT).unwrap());
    match reattach_until_determinate(&mut locked) {
        Reattached::Exited {
            record: None,
            recovery: Recovery::Exited,
        } => {}
        other => panic!("unexpected reattach: {other:?}"),
    }
    assert!(store.journal().unwrap().is_none());
}

#[cfg(feature = "testing")]
#[test]
fn an_unrecorded_vmm_that_serves_its_endpoint_is_ended_on_reattach() {
    let _spawns = serialized();
    let (_directory, store) = sandbox();
    let mut vmm = VmmDirectory::new();
    let mut command = Launcher::orchestrator(&store, vmm.path(), "after-spawn");
    command.env(ORCHESTRATOR_FAKE, "1");
    let mut orchestrator = Launcher::spawn(command);
    let reference = orchestrator.launch_reference();
    vmm.record(&reference);
    assert!(orchestrator.wait().success());
    let process = open_running(&reference);
    // Once the fake OpenVMM serves its endpoint, which names it, reattach verifies that it holds
    // the claimed log, and ends it.
    let mut locked = store.lock(Some(Instant::now() + TIMEOUT)).unwrap();
    match reattach_until_determinate(&mut locked) {
        Reattached::Exited {
            record: None,
            recovery: Recovery::Terminated(terminated),
        } => assert_eq!(terminated, reference),
        other => panic!("unexpected reattach: {other:?}"),
    }
    assert_ended(&process);
    assert!(store.journal().unwrap().is_none());
}

#[test]
fn a_launch_that_crashes_before_it_commits_ends_its_recorded_vmm_on_reattach() {
    let _spawns = serialized();
    let (_directory, store) = sandbox();
    let mut vmm = VmmDirectory::new();
    let mut orchestrator = Launcher::orchestrate(&store, vmm.path(), "after-identity");
    let reference = orchestrator.launch_reference();
    vmm.record(&reference);
    assert!(orchestrator.wait().success());
    let journal = store.journal().unwrap().unwrap();
    assert_eq!(journal.state(), LaunchState::Spawned);
    assert_eq!(journal.process(), Some(&reference));
    let process = open_running(&reference);
    // The VMM is no child of this process, and its verified reference ends it.
    let mut locked = store.lock(Some(Instant::now() + TIMEOUT)).unwrap();
    match supervisor::reattach(&mut locked, &runtime()).unwrap() {
        Reattached::Exited {
            record: None,
            recovery: Recovery::Terminated(terminated),
        } => assert_eq!(terminated, reference),
        other => panic!("unexpected reattach: {other:?}"),
    }
    assert_ended(&process);
    assert!(store.journal().unwrap().is_none());
    assert!(store.read_capability().is_err());
}

#[test]
fn a_committed_vmm_outlives_its_orchestrator_and_stops_gracefully() {
    let _spawns = serialized();
    let (_directory, store) = sandbox();
    let mut vmm = VmmDirectory::new();
    let mut orchestrator = Launcher::orchestrate(&store, vmm.path(), "committed");
    let reference = orchestrator.launch_reference();
    vmm.record(&reference);
    orchestrator.finish();

    // The restarted orchestrator holds the VMM, which is no child of it, again.
    let mut locked = store.lock(Some(Instant::now() + TIMEOUT)).unwrap();
    let Reattached::Live(running) = supervisor::reattach(&mut locked, &runtime()).unwrap() else {
        panic!("the committed VMM was not reattached");
    };
    assert_eq!(running.record().process(), &reference);
    assert_eq!(running.record().runtime(), &runtime());
    assert_eq!(running.process().wait_method(), WaitMethod::Event);
    assert!(!running.wait(Instant::now() + SHORT_WAIT).unwrap());
    assert_eq!(store.read_capability().unwrap(), CAPABILITY);

    // The graceful step asks the VMM to exit through the caller's own channel, and the stop waits
    // for the VMM's exit event.
    let stopped = running
        .stop(
            &mut locked,
            Instant::now() + TIMEOUT,
            None,
            |stopping, _deadline| {
                assert_eq!(stopping.endpoint(), running.endpoint());
                vmm.end();
                Ok::<(), Infallible>(())
            },
        )
        .unwrap();
    assert_eq!(stopped.outcome, StopOutcome::Graceful);
    assert!(stopped.graceful_error.is_none());
    assert_ended(running.process());
    // The stop cleared the runtime record, and kept the log.
    assert!(store.runtime().unwrap().is_none());
    assert!(store.read_capability().is_err());
    assert!(store.log_path().exists());
    match supervisor::reattach(&mut locked, &runtime()).unwrap() {
        Reattached::Exited {
            record: None,
            recovery: Recovery::Clean,
        } => {}
        other => panic!("unexpected reattach: {other:?}"),
    }
}

#[test]
fn a_vmm_of_another_runtime_is_ended_only_by_its_termination() {
    let _spawns = serialized();
    let (_directory, store) = sandbox();
    let (_other_directory, other) = sandbox();
    let mut vmm = VmmDirectory::new();
    let mut locked = store.lock(None).unwrap();
    // The orchestrator that launched the VMM restarts.
    drop(launch_vmm(&mut locked, &mut vmm));

    // Another build of the orchestrator would launch another initramfs.
    let mut upgraded = runtime();
    upgraded.initramfs = artifact('4');
    let Reattached::Mismatched {
        vmm: running,
        differences,
    } = supervisor::reattach(&mut locked, &upgraded).unwrap()
    else {
        panic!("a VMM of another runtime was not reported as mismatched");
    };
    assert_eq!(differences, [IdentityPart::Initramfs]);
    // The VMM heeds no graceful step of this build. Without the forced step, the stop gives up at
    // its deadline, and leaves the VMM running, with its record.
    let ignored = |_: &Vmm, _: Instant| Ok::<(), Infallible>(());
    let stopped = running
        .stop(&mut locked, Instant::now() + SHORT_WAIT, None, ignored)
        .unwrap();
    assert_eq!(stopped.outcome, StopOutcome::TimedOut);
    // A termination through another sandbox's store, or with a bound that is zero or too long,
    // signals nothing.
    let error = running
        .terminate(&mut other.lock(None).unwrap(), TIMEOUT)
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Other, "{error}");
    for bound in [Duration::ZERO, MAX_TIMEOUT + Duration::from_secs(1)] {
        let error = running.terminate(&mut locked, bound).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{bound:?}");
    }
    assert!(!running.has_exited().unwrap());
    assert!(store.runtime().unwrap().is_some());

    // The termination ends it through its verified handle, with no graceful step, and clears its
    // runtime record. The log outlives it.
    assert_eq!(
        running.terminate(&mut locked, TIMEOUT).unwrap(),
        StopOutcome::Forced
    );
    assert_ended(running.process());
    assert!(store.runtime().unwrap().is_none());
    assert!(store.read_capability().is_err());
    assert!(store.log_path().exists());
    // A VMM that exited is not signaled again.
    assert_eq!(
        running.terminate(&mut locked, TIMEOUT).unwrap(),
        StopOutcome::Graceful
    );
    match supervisor::reattach(&mut locked, &upgraded).unwrap() {
        Reattached::Exited {
            record: None,
            recovery: Recovery::Clean,
        } => {}
        other => panic!("unexpected reattach: {other:?}"),
    }
}

#[test]
fn records_of_reused_ids_and_earlier_boots_name_vmms_that_exited() {
    let _spawns = serialized();
    let (_directory, store) = sandbox();
    let mut vmm = VmmDirectory::new();
    let mut locked = store.lock(None).unwrap();
    let launched = launch_vmm(&mut locked, &mut vmm);
    let reference = launched.record().process().clone();
    let path = store.dir().join(RUNTIME_NAME);
    let recorded: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    // The recorded ID with another start time names another process, which reused the ID after
    // the VMM exited, and a record of another boot names a VMM that exited with that boot. Under
    // another runtime identity too, neither is mismatched, which would end it: only a VMM that
    // reattach verified is.
    let mut other_boot = *reference.boot.as_bytes();
    other_boot[0] ^= 1;
    let mut upgraded = runtime();
    upgraded.kernel = artifact('5');
    for (field, value) in [
        ("startTime", json!(reference.start_time + 1)),
        ("boot", json!(BootId::from_bytes(other_boot))),
    ] {
        let mut tampered = recorded.clone();
        tampered["process"][field] = value;
        fs::write(&path, serde_json::to_vec(&tampered).unwrap()).unwrap();
        let Reattached::Exited {
            record: Some(record),
            recovery: Recovery::Clean,
        } = supervisor::reattach(&mut locked, &upgraded).unwrap()
        else {
            panic!("a record with another {field} named a live VMM");
        };
        assert_ne!(record.process(), &reference);
        assert!(store.runtime().unwrap().is_none());
        // The process that has the ID was not signaled.
        assert!(!launched.has_exited().unwrap());
    }
    // The true record still names the VMM.
    fs::write(&path, serde_json::to_vec(&recorded).unwrap()).unwrap();
    let Reattached::Live(running) = supervisor::reattach(&mut locked, &runtime()).unwrap() else {
        panic!("the recorded VMM was not reattached");
    };
    let stopped = running
        .stop(
            &mut locked,
            Instant::now() + TIMEOUT,
            Some(TIMEOUT),
            |_, _| {
                vmm.end();
                Ok::<(), Infallible>(())
            },
        )
        .unwrap();
    assert_eq!(stopped.outcome, StopOutcome::Graceful);
}

/// Sets its flag when dropped, also when its thread panics.
struct SetOnDrop<'a>(&'a AtomicBool);

impl Drop for SetOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[test]
fn vmms_inherit_only_their_streams_and_allowed_descriptors_while_others_spawn() {
    let _spawns = serialized();
    /// The VMMs that the test launches.
    const VMMS: usize = 6;
    let (readers, writers): (Vec<PipeReader>, Vec<PipeWriter>) =
        (0..VMMS).map(|_| io::pipe().unwrap()).unzip();
    // Only the VMM that inherits one of these pipes may hold it.
    #[cfg(target_os = "linux")]
    let forbidden: Vec<PathBuf> = writers
        .iter()
        .map(|writer| this_object(number_of(writer)))
        .collect();
    #[cfg(windows)]
    let forbidden: Vec<PathBuf> = Vec::new();
    let done = AtomicBool::new(false);
    let rounds = AtomicUsize::new(0);
    let others = Mutex::new(Vec::new());
    let leaks = thread::scope(|scope| {
        let stop_spawner = SetOnDrop(&done);
        let spawner = scope.spawn(|| spawn_others(&done, &rounds, &others, &forbidden));
        // The other thread spawns throughout the launches of the VMMs.
        let deadline = Instant::now() + TIMEOUT;
        while rounds.load(Ordering::SeqCst) == 0 {
            assert!(
                Instant::now() < deadline && !spawner.is_finished(),
                "the other thread spawned nothing"
            );
            thread::sleep(Duration::from_millis(10));
        }
        for (reader, writer) in readers.into_iter().zip(&writers) {
            check_vmm(reader, writer, &others);
        }
        drop(stop_spawner);
        spawner.join().unwrap()
    });
    assert!(rounds.load(Ordering::SeqCst) > 0);
    assert!(leaks.is_empty(), "{leaks:?}");
}

/// Spawns processes until `done`, as other threads of an orchestrator do, and counts its rounds
/// in `rounds`. Each round spawns a VMM fixture through the supervisor, which inherits a pipe of
/// its own, and a process through `std::process`, which inherits another pipe, which the round
/// makes inheritable for the spawn, as code that spawns without an allowlist does.
///
/// It lists the numbers of those pipes in `others`, and keeps the pipes open until it returns.
/// It returns the processes that it spawned through `std::process` that held a descriptor of any
/// of the objects `forbidden`.
fn spawn_others(
    done: &AtomicBool,
    rounds: &AtomicUsize,
    others: &Mutex<Vec<usize>>,
    forbidden: &[PathBuf],
) -> Vec<String> {
    // Without the VMM marker, the fixtures that it spawns return at once.
    let directory = tempfile::tempdir().unwrap();
    let mut pipes = Vec::new();
    let mut leaks = Vec::new();
    while !done.load(Ordering::SeqCst) {
        let (_, allowed) = io::pipe().unwrap();
        let (_, naive) = io::pipe().unwrap();
        others
            .lock()
            .unwrap()
            .extend([number_of(&allowed), number_of(&naive)]);

        let mut command = VmmCommand::new(
            env::current_exe().unwrap(),
            fixture_arguments("vmm_fixture"),
            directory.path(),
        );
        command.inherit.push(Inherited::new(&allowed));
        let round = rounds.load(Ordering::SeqCst);
        let log = File::create(directory.path().join(format!("{round}.log"))).unwrap();
        assert!(
            supervisor::spawn(&command, &CAPABILITY, log)
                .unwrap()
                .kill_and_wait()
        );

        set_inheritable(&naive, true);
        let child = sleeper().spawn();
        set_inheritable(&naive, false);
        let mut child = child.unwrap();
        leaks.extend(
            held_objects(child.id(), forbidden)
                .into_iter()
                .map(|object| format!("process {} held {}", child.id(), object.display())),
        );
        let _ = child.kill();
        child.wait().unwrap();
        pipes.extend([allowed, naive]);
        rounds.fetch_add(1, Ordering::SeqCst);
    }
    leaks
}

/// Spawns a VMM fixture that inherits `writer` while another thread spawns processes whose pipes
/// `others` lists, and checks that the VMM holds its standard streams and `writer`, under its
/// number, and nothing else of this process's.
fn check_vmm(reader: PipeReader, writer: &PipeWriter, others: &Mutex<Vec<usize>>) {
    let mut vmm = VmmDirectory::new();
    let number = number_of(writer);
    let mut command = vmm_command(vmm.path(), false);
    command
        .arguments
        .push(format!("{INHERITED_PREFIX}{number}").into());
    command.inherit.push(Inherited::new(writer));
    let log = File::create(vmm.path().join("vmm.log")).unwrap();
    let launched = supervisor::spawn(&command, &CAPABILITY, log).unwrap();
    let process = launched.process().unwrap();
    vmm.record(process.reference());
    // The VMM writes through its pipe, under the number that its arguments name.
    assert_eq!(read_line(reader), format!("inherited {number}\n"));
    #[cfg(target_os = "linux")]
    {
        let _ = others;
        let reported = reported_descriptors(vmm.path());
        let numbers: Vec<usize> = reported.iter().map(|(number, _)| *number).collect();
        assert_eq!(numbers, [0, 1, 2, number], "{reported:?}");
        let log = fs::canonicalize(vmm.path().join("vmm.log")).unwrap();
        assert!(
            reported[0].1.to_string_lossy().starts_with("pipe:"),
            "{reported:?}"
        );
        assert_eq!(reported[1].1, log);
        assert_eq!(reported[2].1, log);
        assert_eq!(reported[3].1, this_object(number));
    }
    #[cfg(windows)]
    {
        let pid = launched.id();
        assert!(holds(pid, number), "the VMM did not inherit its pipe");
        for &other in others.lock().unwrap().iter() {
            assert!(
                !holds(pid, other),
                "the VMM inherited handle {other} of another spawn"
            );
        }
    }
    vmm.end();
    assert!(process.wait(Instant::now() + TIMEOUT).unwrap());
    assert!(launched.kill_and_wait());
}

/// Reads a line from `reader` within [`TIMEOUT`].
fn read_line(reader: PipeReader) -> String {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(reader).read_line(&mut line);
        let _ = sender.send(line);
    });
    receiver
        .recv_timeout(TIMEOUT)
        .expect("the VMM wrote no line in time")
}

/// Returns the number of the descriptor of `pipe`, which a VMM inherits under that number.
#[cfg(target_os = "linux")]
fn number_of(pipe: &PipeWriter) -> usize {
    use std::os::fd::AsRawFd;

    usize::try_from(pipe.as_raw_fd()).unwrap()
}

/// Returns the value of the handle of `pipe`, which a VMM inherits under that value.
#[cfg(windows)]
fn number_of(pipe: &PipeWriter) -> usize {
    use std::os::windows::io::AsRawHandle;

    pipe.as_raw_handle() as usize
}

/// Returns the object that the descriptor `number` of this process holds, as its link in `/proc`
/// names it.
#[cfg(target_os = "linux")]
fn this_object(number: usize) -> PathBuf {
    fs::read_link(format!("/proc/self/fd/{number}")).unwrap()
}

/// Waits for the VMM fixture in `directory` to report its descriptors, and returns them, with the
/// objects that they hold.
#[cfg(target_os = "linux")]
fn reported_descriptors(directory: &Path) -> Vec<(usize, PathBuf)> {
    let path = directory.join(DESCRIPTORS_REPORT);
    let deadline = Instant::now() + TIMEOUT;
    let report = loop {
        match fs::read_to_string(&path) {
            Ok(report) => break report,
            Err(error) if error.kind() == io::ErrorKind::NotFound && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("the VMM reported no descriptors: {error}"),
        }
    };
    report
        .lines()
        .map(|line| {
            let (number, object) = line.split_once(' ').unwrap();
            (number.parse().unwrap(), PathBuf::from(object))
        })
        .collect()
}

/// Returns the objects of `forbidden` that process `pid` holds a descriptor of.
#[cfg(target_os = "linux")]
fn held_objects(pid: u32, forbidden: &[PathBuf]) -> Vec<PathBuf> {
    fs::read_dir(format!("/proc/{pid}/fd"))
        .unwrap()
        .filter_map(|entry| fs::read_link(entry.unwrap().path()).ok())
        .filter(|object| forbidden.contains(object))
        .collect()
}

/// Returns the objects of `forbidden` that process `pid` holds a handle of: none, since a process
/// that another thread creates while a VMM is created can inherit the VMM's handles on Windows, as
/// [`Inherited`] documents, so no object is forbidden there.
#[cfg(windows)]
fn held_objects(_pid: u32, forbidden: &[PathBuf]) -> Vec<PathBuf> {
    assert!(forbidden.is_empty());
    Vec::new()
}

/// Returns whether the handle `value` of process `pid` refers to the object of this process's
/// handle `value`, as a handle that the process inherited from this one does.
#[cfg(windows)]
fn holds(pid: u32, value: usize) -> bool {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use std::ptr;

    use windows_sys::Win32::Foundation::{
        CompareObjectHandles, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, PROCESS_DUP_HANDLE,
    };

    let ours = value as HANDLE;
    // SAFETY: OpenProcess has no memory-safety preconditions.
    let process = unsafe { OpenProcess(PROCESS_DUP_HANDLE, 0, pid) };
    assert!(
        !process.is_null(),
        "cannot open process {pid}: {}",
        io::Error::last_os_error()
    );
    // SAFETY: the handle is new, and nothing else owns it.
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    let mut copy = ptr::null_mut();
    // SAFETY: the process handle may duplicate the process's handles, the value is looked up in
    // that process only, and the output pointer is valid.
    let duplicated = unsafe {
        DuplicateHandle(
            std::os::windows::io::AsRawHandle::as_raw_handle(&process),
            ours,
            GetCurrentProcess(),
            &mut copy,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    } != 0;
    // A value that names no handle of the process names none of ours.
    if !duplicated {
        return false;
    }
    // SAFETY: the copy is new, and nothing else owns it.
    let copy = unsafe { OwnedHandle::from_raw_handle(copy) };
    // SAFETY: both handles are open.
    unsafe {
        CompareObjectHandles(
            std::os::windows::io::AsRawHandle::as_raw_handle(&copy),
            ours,
        ) != 0
    }
}

/// Makes `pipe` inheritable, or not, as code that spawns without an allowlist does around its
/// spawns.
#[cfg(target_os = "linux")]
fn set_inheritable(pipe: &PipeWriter, inheritable: bool) {
    use nix::fcntl::{FcntlArg, FdFlag, fcntl};

    let flags = if inheritable {
        FdFlag::empty()
    } else {
        FdFlag::FD_CLOEXEC
    };
    fcntl(pipe, FcntlArg::F_SETFD(flags)).unwrap();
}

/// Makes `pipe` inheritable, or not, as code that spawns without an allowlist does around its
/// spawns.
#[cfg(windows)]
fn set_inheritable(pipe: &PipeWriter, inheritable: bool) {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation};

    let flags = if inheritable { HANDLE_FLAG_INHERIT } else { 0 };
    // SAFETY: the handle is open.
    let set =
        unsafe { SetHandleInformation(pipe.as_raw_handle() as HANDLE, HANDLE_FLAG_INHERIT, flags) };
    assert_ne!(set, 0, "{}", io::Error::last_os_error());
}

/// Returns a command that runs for a few seconds, without standard streams.
fn sleeper() -> Command {
    let mut command = if cfg!(windows) {
        let mut command = Command::new("ping");
        command.args(["-n", "5", "127.0.0.1"]);
        command
    } else {
        let mut command = Command::new("sleep");
        command.arg("5");
        command
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

#[cfg(windows)]
#[test]
fn a_vmm_that_breaks_away_survives_its_orchestrators_job() {
    let _spawns = serialized();
    let (_directory, store) = sandbox();
    let mut vmm = VmmDirectory::new();
    let job = job::Job::new(true);
    let mut command = Launcher::orchestrator(&store, vmm.path(), "committed");
    command.env(LAUNCHER_BREAKAWAY, "1");
    let mut orchestrator = Launcher::spawn(command);
    job.assign(&orchestrator.child);
    let reference = orchestrator.launch_reference();
    vmm.record(&reference);
    // The orchestrator crashes with its job, and its VMM, which broke away, survives it.
    drop(job);
    orchestrator.wait();
    let mut locked = store.lock(Some(Instant::now() + TIMEOUT)).unwrap();
    let Reattached::Live(running) = supervisor::reattach(&mut locked, &runtime()).unwrap() else {
        panic!("the VMM that broke away was not reattached");
    };
    assert_eq!(running.record().process(), &reference);
    let stopped = running
        .stop(&mut locked, Instant::now() + TIMEOUT, None, |_, _| {
            vmm.end();
            Ok::<(), Infallible>(())
        })
        .unwrap();
    assert_eq!(stopped.outcome, StopOutcome::Graceful);
    assert!(store.runtime().unwrap().is_none());
}

#[cfg(windows)]
#[test]
fn a_vmm_in_its_orchestrators_job_ends_with_it() {
    let _spawns = serialized();
    let (_directory, store) = sandbox();
    let mut vmm = VmmDirectory::new();
    let job = job::Job::new(false);
    let mut orchestrator = Launcher::orchestrate(&store, vmm.path(), "committed");
    job.assign(&orchestrator.child);
    let reference = orchestrator.launch_reference();
    vmm.record(&reference);
    let process = open_running(&reference);
    // The VMM did not break away, so closing the job ends it with its orchestrator.
    drop(job);
    orchestrator.wait();
    assert!(process.wait(Instant::now() + TIMEOUT).unwrap());
    let mut locked = store.lock(Some(Instant::now() + TIMEOUT)).unwrap();
    match supervisor::reattach(&mut locked, &runtime()).unwrap() {
        Reattached::Exited {
            record: Some(record),
            recovery: Recovery::Clean,
        } => assert_eq!(record.process(), &reference),
        other => panic!("unexpected reattach: {other:?}"),
    }
    assert!(store.runtime().unwrap().is_none());
}

#[cfg(windows)]
#[test]
fn a_job_that_ends_its_vmm_wakes_the_waiters() {
    let _spawns = serialized();
    let mut vmm = VmmDirectory::new();
    let job = job::Job::new(false);
    let mut launcher = Launcher::start(vmm.path(), false);
    job.assign(&launcher.child);
    let reference = launcher.launch_reference();
    vmm.record(&reference);
    // The VMM did not break away, so the job holds it too, and closing the job ends it.
    let process = open_running(&reference);
    assert!(!process.wait(Instant::now() + SHORT_WAIT).unwrap());
    let started = Instant::now();
    drop(job);
    assert!(process.wait(started + TIMEOUT).unwrap());
    assert_ended(&process);
    launcher.wait();
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

    // A sandbox whose runtime record names it cannot tell whether its VMM runs: reattach keeps
    // the record, and signals nothing.
    let (_directory, store) = sandbox();
    write_record(&store, &reference);
    let mut locked = store.lock(None).unwrap();
    let Reattached::Indeterminate(reason) = supervisor::reattach(&mut locked, &runtime()).unwrap()
    else {
        panic!("a VMM whose identity cannot be checked was classified");
    };
    assert!(reason.contains("cannot be examined"), "{reason}");
    assert!(store.runtime().unwrap().is_some());
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
    assert!(process.wait(Instant::now() + TIMEOUT).unwrap());
    process.kill().unwrap();
    assert_ended(&process);
}

#[cfg(windows)]
#[test]
fn a_vmm_of_another_runtime_that_cannot_be_terminated_keeps_its_record() {
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
    // Another build of the orchestrator, which would launch another kernel, finds the VM.
    let (_directory, store) = sandbox();
    write_record(&store, &reference);
    let mut upgraded = runtime();
    upgraded.kernel = artifact('5');
    let mut locked = store.lock(None).unwrap();
    let Reattached::Mismatched { vmm, differences } =
        supervisor::reattach(&mut locked, &upgraded).unwrap()
    else {
        panic!("a VMM of another runtime was not reported as mismatched");
    };
    assert_eq!(differences, [IdentityPart::Kernel]);
    // The termination fails, and reports it: the VMM still runs, and keeps its record.
    let error = vmm.terminate(&mut locked, TIMEOUT).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert_eq!(
        error.message(),
        format!("cannot terminate VMM process {}", reference.pid)
    );
    assert!(store.runtime().unwrap().is_some());
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "the process was ended"
    );

    // Once the VMM has exited, the next termination finds it so, and clears the record.
    child.0.kill().unwrap();
    assert!(vmm.wait(Instant::now() + TIMEOUT).unwrap());
    assert_eq!(
        vmm.terminate(&mut locked, TIMEOUT).unwrap(),
        StopOutcome::Graceful
    );
    assert!(store.runtime().unwrap().is_none());
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

/// Job objects, which Windows orchestrators run in.
#[cfg(windows)]
mod job {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::process::Child;
    use std::{io, mem, ptr};

    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };

    /// A job object that terminates its processes when the test closes it.
    pub(crate) struct Job(OwnedHandle);

    impl Job {
        /// Creates a job that terminates its processes when it closes, and that lets them start
        /// processes outside it if `breakaway`.
        pub(crate) fn new(breakaway: bool) -> Self {
            // SAFETY: creates an unnamed job object with default security.
            let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
            assert!(
                !handle.is_null(),
                "cannot create a job object: {}",
                io::Error::last_os_error()
            );
            // SAFETY: the handle is new, and nothing else owns it.
            let job = Self(unsafe { OwnedHandle::from_raw_handle(handle) });
            // SAFETY: the structure is plain data for which all-zero bytes are valid.
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                | if breakaway {
                    JOB_OBJECT_LIMIT_BREAKAWAY_OK
                } else {
                    0
                };
            // SAFETY: the handle is a job object, and the information has the size given.
            let set = unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    (&raw const limits).cast(),
                    mem::size_of_val(&limits) as u32,
                )
            };
            assert_ne!(
                set,
                0,
                "cannot set the limits of a job object: {}",
                io::Error::last_os_error()
            );
            job
        }

        /// Places `child` in the job, before it starts any process of its own.
        pub(crate) fn assign(&self, child: &Child) {
            // SAFETY: both handles are open for the duration of the call.
            let assigned =
                unsafe { AssignProcessToJobObject(self.0.as_raw_handle(), child.as_raw_handle()) };
            assert_ne!(
                assigned,
                0,
                "cannot place the launcher in a job object: {}",
                io::Error::last_os_error()
            );
        }
    }
}
