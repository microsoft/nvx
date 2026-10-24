//! The sandbox store across processes, on Linux and Windows: records that another build wrote,
//! and launches that crash at each step, which recovery ends without signaling an unverified
//! process. These are the host parts of the crash-recovery and runtime-identity scenarios.
//!
//! The crashes are real: a child process of this test binary, the orchestrator, launches a VMM
//! stand-in in a store and exits without unwinding at the chosen step, as a crash would. The VMM
//! stand-in is this test binary too, which reads its capability and sleeps, or, with the
//! `testing` feature, the fake OpenVMM, which also serves the control endpoint.
#![cfg(all(feature = "host", any(target_os = "linux", windows)))]

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use nvx::ErrorKind;
use nvx::host::platform;
use nvx::host::process::{BootId, Process, ProcessRef};
use nvx::host::store::{
    ArtifactIdentity, IdentityCheck, IdentityPart, JOURNAL_NAME, Launch, LaunchId, LaunchState,
    LockedStore, RUNTIME_NAME, Recovery, RuntimeIdentity, SandboxStore,
};
use nvx::host::supervisor::{self, Launched, VmmCommand};
use nvx::protocol::CAPABILITY_LEN;

/// The capability of every launch of these tests.
const CAPABILITY: [u8; CAPABILITY_LEN] = [7; CAPABILITY_LEN];
/// The argument that tells [`vmm_role`] that a test spawned it.
const VMM_MARKER: &str = "nvx-store-test-vmm";
/// The start of the line in which [`orchestrator_role`] prints the process ID of its VMM.
const VMM_LINE: &str = "nvx-store-test-vmm-pid ";
/// The store in which [`orchestrator_role`] launches.
const ORCHESTRATOR_STORE: &str = "NVX_STORE_TEST_STORE";
/// The step after which [`orchestrator_role`] crashes: `before-spawn`, `after-spawn`, or
/// `after-identity`.
const ORCHESTRATOR_CRASH: &str = "NVX_STORE_TEST_CRASH";
/// The boot delay, in milliseconds, of the fake OpenVMM that [`orchestrator_role`] launches
/// instead of a VMM stand-in, if set.
const ORCHESTRATOR_FAKE: &str = "NVX_STORE_TEST_FAKE_BOOT_DELAY_MS";
/// The store that [`restrictive_umask_role`] creates.
#[cfg(target_os = "linux")]
const UMASK_STORE: &str = "NVX_STORE_TEST_UMASK_STORE";
/// The umask, in octal, of [`restrictive_umask_role`].
#[cfg(target_os = "linux")]
const UMASK_VALUE: &str = "NVX_STORE_TEST_UMASK";
/// The longest wait for a recovery to become determinate.
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(30);

/// Serializes the tests that spawn processes: a process that one test spawns while another test's
/// log is inheritable for its own spawn could hold that log's claim for a while.
static SPAWNS: Mutex<()> = Mutex::new(());

/// Takes [`SPAWNS`].
fn serialized() -> MutexGuard<'static, ()> {
    SPAWNS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Returns the identity `sha256:` and 64 copies of `digit`.
fn artifact(digit: char) -> ArtifactIdentity {
    ArtifactIdentity::sha256(&digit.to_string().repeat(64)).unwrap()
}

/// Returns the runtime identity of these tests' launches, and of the fixtures.
fn runtime() -> RuntimeIdentity {
    RuntimeIdentity::new(artifact('1'), artifact('2'), artifact('3'))
}

/// Returns a store in a fresh temporary directory.
fn store() -> (tempfile::TempDir, SandboxStore) {
    let directory = tempfile::tempdir().unwrap();
    let store = SandboxStore::create(directory.path().join("sandbox")).unwrap();
    (directory, store)
}

/// Returns the path of the fixture `name`.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("store")
        .join(name)
}

/// Installs the fixture `name` in `store` as its file `as_name`.
fn install(store: &SandboxStore, name: &str, as_name: &str) {
    fs::copy(fixture(name), store.dir().join(as_name)).unwrap();
}

/// Returns the program and arguments that run the ignored test `name` of this binary alone.
fn role(name: &str, extra: &[&str]) -> (PathBuf, Vec<OsString>) {
    let arguments = [
        "--exact",
        name,
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]
    .iter()
    .chain(extra)
    .map(OsString::from)
    .collect();
    (env::current_exe().unwrap(), arguments)
}

/// Spawns a VMM stand-in that holds the log of `launch`.
fn spawn_vmm(launch: &mut Launch<'_>, store: &SandboxStore) -> Launched {
    let (program, arguments) = role("vmm_role", &[VMM_MARKER]);
    let log = launch.take_log().unwrap();
    supervisor::spawn(
        &VmmCommand::new(program, arguments, store.dir()),
        &CAPABILITY,
        log,
    )
    .unwrap()
}

/// Spawns the fake OpenVMM, which serves the endpoint of `launch` after `boot_delay_ms`, with the
/// log of `launch`.
#[cfg(feature = "testing")]
fn spawn_fake(launch: &mut Launch<'_>, store: &SandboxStore, boot_delay_ms: u64) -> Launched {
    use nvx::host::launch::LaunchSpec;
    use nvx::{Artifacts, Hypervisor};

    let directory = store.dir().parent().unwrap();
    fs::write(directory.join("vmlinux"), b"kernel").unwrap();
    fs::write(directory.join("initramfs.cpio.gz"), b"initrd").unwrap();
    let artifacts = Artifacts::new(
        env!("CARGO_BIN_EXE_nvx-fake-openvmm"),
        directory.join("vmlinux"),
        directory.join("initramfs.cpio.gz"),
    );
    let mut spec = LaunchSpec::new(artifacts, Hypervisor::platform_default().unwrap());
    spec.kernel_parameters = format!("fake_boot_delay_ms={boot_delay_ms}");
    let arguments = spec
        .openvmm_arguments(launch.endpoint(), &store.outcome_path())
        .unwrap();
    let log = launch.take_log().unwrap();
    supervisor::spawn(
        &VmmCommand::new(&spec.openvmm, arguments, store.dir()),
        &CAPABILITY,
        log,
    )
    .unwrap()
}

/// A VMM stand-in that a test spawned, which is killed when the test ends.
struct Vmm(Option<Launched>);

impl Vmm {
    /// Returns the stand-in's process ID.
    fn id(&self) -> u32 {
        self.0.as_ref().unwrap().id()
    }
}

impl Drop for Vmm {
    fn drop(&mut self) {
        if let Some(launched) = self.0.take() {
            launched.kill_and_wait();
        }
    }
}

/// Returns whether the process `pid` still runs with the start time `start_time`.
fn runs(pid: u32, start_time: u64) -> bool {
    platform::process_start_time(pid).unwrap() == Some(start_time)
}

/// Ends the process `pid` that started at `start_time` in this boot, through its verified
/// handle, if it still runs, and returns whether it exited within [`RECOVERY_TIMEOUT`].
fn kill(pid: u32, start_time: u64) -> io::Result<bool> {
    let reference = ProcessRef {
        pid,
        start_time,
        boot: BootId::current()?,
    };
    let Some(process) = Process::open(&reference)? else {
        return Ok(true);
    };
    process.kill()?;
    process.wait(Instant::now() + RECOVERY_TIMEOUT)
}

/// A VMM that a crashed orchestrator left behind, which is killed when the test ends if it still
/// runs.
struct Orphan {
    /// The VMM's process ID.
    pid: u32,
    /// The VMM's start time.
    start_time: u64,
}

impl Orphan {
    /// Returns whether the VMM still runs.
    fn runs(&self) -> bool {
        runs(self.pid, self.start_time)
    }
}

impl Drop for Orphan {
    fn drop(&mut self) {
        let _ = kill(self.pid, self.start_time);
    }
}

/// Runs an orchestrator that launches in `store` and crashes after the step `crash`, launching
/// the fake OpenVMM with `fake_boot_delay_ms`, if given, or else a VMM stand-in. Returns the VMM
/// that it left behind, if it spawned one.
fn crash_launch(
    store: &SandboxStore,
    crash: &str,
    fake_boot_delay_ms: Option<u64>,
) -> Option<Orphan> {
    let (program, arguments) = role("orchestrator_role", &[]);
    let mut command = Command::new(program);
    // The orchestrator names its store relative to its working directory.
    command
        .args(arguments)
        .current_dir(store.dir().parent().unwrap())
        .env(ORCHESTRATOR_STORE, store.dir().file_name().unwrap())
        .env(ORCHESTRATOR_CRASH, crash)
        .stdin(Stdio::null());
    if let Some(delay) = fake_boot_delay_ms {
        command.env(ORCHESTRATOR_FAKE, delay.to_string());
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "the orchestrator failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let pid: u32 = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix(VMM_LINE))?
        .trim()
        .parse()
        .unwrap();
    let start_time = platform::process_start_time(pid)
        .unwrap()
        .expect("the VMM that the orchestrator left behind runs");
    Some(Orphan { pid, start_time })
}

/// Recovers until recovery is determinate, as a caller retries an indeterminate recovery, and
/// returns how it ended the launch.
///
/// On Windows, a scanner that briefly opens the log can keep its claim held for a moment.
fn recover_until_determinate(locked: &mut LockedStore) -> Recovery {
    let deadline = Instant::now() + RECOVERY_TIMEOUT;
    loop {
        match locked.recover().unwrap() {
            Recovery::Indeterminate(reason) if Instant::now() < deadline => {
                eprintln!("recovery is indeterminate: {reason}");
                thread::sleep(Duration::from_millis(100));
            }
            recovery => return recovery,
        }
    }
}

/// The orchestrator that [`crash_launch`] runs in a process of its own: it begins a launch in
/// the store that [`ORCHESTRATOR_STORE`] names, and exits without unwinding after the step that
/// [`ORCHESTRATOR_CRASH`] names, as a crash would. It prints [`VMM_LINE`] and the VMM's process
/// ID once it has spawned the VMM.
#[test]
#[ignore = "an orchestrator that the store tests run in a process of its own"]
fn orchestrator_role() {
    let Some(directory) = env::var_os(ORCHESTRATOR_STORE) else {
        return;
    };
    let crash = env::var(ORCHESTRATOR_CRASH).unwrap();
    let crash_now = || -> ! {
        io::stdout().flush().unwrap();
        std::process::exit(0)
    };
    let store = SandboxStore::open(directory).unwrap();
    // The store resolves its relative path, so that the endpoint that the VMM listens on, in
    // another working directory, and that recovery examines, is the same.
    assert!(store.dir().is_absolute());
    let mut locked = store.lock(None).unwrap();
    let mut launch = locked
        .begin_launch(&LaunchId::generate().unwrap(), &runtime(), &CAPABILITY)
        .unwrap();
    if crash == "before-spawn" {
        crash_now();
    }
    let child = match env::var(ORCHESTRATOR_FAKE) {
        #[cfg(feature = "testing")]
        Ok(delay) => spawn_fake(&mut launch, &store, delay.parse().unwrap()),
        _ => spawn_vmm(&mut launch, &store),
    };
    // The test harness has printed the test's name without ending the line.
    println!("\n{VMM_LINE}{}", child.id());
    if crash == "after-spawn" {
        crash_now();
    }
    launch.record_process(&child).unwrap();
    if crash == "after-identity" {
        crash_now();
    }
    panic!("unknown crash step {crash}");
}

/// The VMM stand-in: it reads the capability that a launch gives it and sleeps, holding the log
/// that it inherited as its standard output and error.
#[test]
#[ignore = "a VMM stand-in that the store tests spawn"]
fn vmm_role() {
    if !env::args().any(|argument| argument == VMM_MARKER) {
        return;
    }
    let mut capability = [0; CAPABILITY_LEN];
    io::stdin().read_exact(&mut capability).unwrap();
    assert_eq!(capability, CAPABILITY);
    // Bounded, so that a stand-in that a failed test leaves behind exits on its own.
    thread::sleep(Duration::from_secs(60));
}

/// A user of the store whose umask clears even the current user's permission to read or write
/// the files and directories that it creates. The store sets their modes again, so that its
/// records, its capability, and its lock still open, as their next launch and OpenVMM need. The
/// umask belongs to the whole process, so this runs in a process of its own.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "a store user that the store tests run in a process of its own"]
fn restrictive_umask_role() {
    use std::os::unix::fs::PermissionsExt;

    use nix::sys::stat::{Mode, umask};

    let (Some(directory), Some(mask)) = (env::var_os(UMASK_STORE), env::var_os(UMASK_VALUE)) else {
        return;
    };
    let mask = u32::from_str_radix(mask.to_str().unwrap(), 8).unwrap();
    umask(Mode::from_bits_truncate(mask));
    let store = SandboxStore::create(directory).unwrap();
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode(store.dir()), 0o700);
    let lock = store.dir().join(nvx::host::store::LOCK_NAME);
    // The second launch opens the lock file again and replaces the first launch's files.
    for _ in 0..2 {
        let mut locked = store.lock(None).unwrap();
        let launch = locked
            .begin_launch(&LaunchId::generate().unwrap(), &runtime(), &CAPABILITY)
            .unwrap();
        assert_eq!(store.journal().unwrap().as_ref(), Some(launch.journal()));
        assert_eq!(store.read_capability().unwrap(), CAPABILITY);
        for path in [
            lock.clone(),
            store.dir().join(JOURNAL_NAME),
            store.capability_path(),
            store.log_path(),
        ] {
            assert_eq!(mode(&path), 0o600, "{}", path.display());
        }
        assert_eq!(launch.abandon().unwrap(), Recovery::Exited);
    }
    // A caller that crashed before it set the mode of the lock file that it created left it so.
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o000)).unwrap();
    drop(store.lock(None).unwrap());
    assert_eq!(mode(&lock), 0o600);
}

#[cfg(target_os = "linux")]
#[test]
fn a_restrictive_umask_leaves_the_store_usable() {
    for mask in ["400", "200", "777"] {
        let directory = tempfile::tempdir().unwrap();
        let (program, arguments) = role("restrictive_umask_role", &[]);
        let status = Command::new(program)
            .args(arguments)
            .env(UMASK_STORE, directory.path().join("sandbox"))
            .env(UMASK_VALUE, mask)
            .status()
            .unwrap();
        assert!(status.success(), "umask {mask}");
    }
}

#[test]
fn a_crash_before_the_spawn_leaves_a_released_claim() {
    let _spawns = serialized();
    let (_directory, store) = store();
    assert!(crash_launch(&store, "before-spawn", None).is_none());
    let journal = store.journal().unwrap().unwrap();
    assert_eq!(journal.state(), LaunchState::Prepared);
    // The crash released the orchestrator's lock, and the claim of its unused log.
    let mut locked = store.lock(Some(Instant::now() + RECOVERY_TIMEOUT)).unwrap();
    assert_eq!(recover_until_determinate(&mut locked), Recovery::Exited);
    assert!(store.journal().unwrap().is_none());
    assert!(store.read_capability().is_err());
    assert!(store.log_path().exists());
}

#[test]
fn the_log_claim_proves_that_an_unrecorded_vmm_exited() {
    let _spawns = serialized();
    let (_directory, store) = store();
    let vmm = crash_launch(&store, "after-spawn", None).unwrap();
    assert_eq!(
        store.journal().unwrap().unwrap().state(),
        LaunchState::Prepared
    );
    let mut locked = store.lock(Some(Instant::now() + RECOVERY_TIMEOUT)).unwrap();
    // The VMM holds the claim, and serves no endpoint that would name it, so recovery can neither
    // prove that it exited nor verify it, and keeps the journal without signaling it.
    let Recovery::Indeterminate(reason) = locked.recover().unwrap() else {
        panic!("recovery ended a launch whose VMM runs unverified");
    };
    assert!(
        reason.contains("no process serves its control endpoint yet"),
        "{reason}"
    );
    assert_eq!(
        store.journal().unwrap().unwrap().state(),
        LaunchState::Prepared
    );
    assert!(store.read_capability().is_ok());
    assert!(vmm.runs());
    // Once the VMM exits, its released claim proves it.
    assert!(kill(vmm.pid, vmm.start_time).unwrap());
    assert_eq!(recover_until_determinate(&mut locked), Recovery::Exited);
    assert!(store.journal().unwrap().is_none());
}

#[test]
fn a_crash_before_the_endpoint_ends_the_recorded_vmm() {
    let _spawns = serialized();
    let (_directory, store) = store();
    let vmm = crash_launch(&store, "after-identity", None).unwrap();
    let journal = store.journal().unwrap().unwrap();
    assert_eq!(journal.state(), LaunchState::Spawned);
    let process = journal.process().unwrap().clone();
    assert_eq!((process.pid, process.start_time), (vmm.pid, vmm.start_time));
    // The VMM is not the recovering process's child.
    let mut locked = store.lock(Some(Instant::now() + RECOVERY_TIMEOUT)).unwrap();
    assert_eq!(locked.recover().unwrap(), Recovery::Terminated(process));
    assert!(!vmm.runs());
    assert!(store.journal().unwrap().is_none());
    assert!(store.read_capability().is_err());
}

#[test]
fn recovery_never_signals_a_process_that_the_journal_does_not_name() {
    let _spawns = serialized();
    let (_directory, store) = store();
    let mut locked = store.lock(None).unwrap();
    let mut launch = locked
        .begin_launch(&LaunchId::generate().unwrap(), &runtime(), &CAPABILITY)
        .unwrap();
    let vmm = Vmm(Some(spawn_vmm(&mut launch, &store)));
    let recorded = launch
        .record_process(vmm.0.as_ref().unwrap())
        .unwrap()
        .reference()
        .clone();
    assert_eq!(recorded.pid, vmm.id());
    // The launch is interrupted.
    drop(launch);
    let journal: serde_json::Value =
        serde_json::from_slice(&fs::read(store.dir().join(JOURNAL_NAME)).unwrap()).unwrap();
    let tamper = |field: &str, value: serde_json::Value| {
        let mut tampered = journal.clone();
        tampered["process"][field] = value;
        fs::write(
            store.dir().join(JOURNAL_NAME),
            serde_json::to_vec(&tampered).unwrap(),
        )
        .unwrap();
    };
    // The recorded process ID with another start time, as when the ID is reused, names no live
    // process. The VMM still holds the launch's log, which shows that the launch may still run,
    // so recovery signals nothing and keeps the journal.
    tamper("startTime", serde_json::json!(recorded.start_time + 1));
    let Recovery::Indeterminate(reason) = locked.recover().unwrap() else {
        panic!("recovery ended a launch whose log is still held");
    };
    assert!(reason.contains("still holds its log"), "{reason}");
    assert!(store.journal().unwrap().is_some());
    assert!(runs(recorded.pid, recorded.start_time));
    // A reference from another boot, as after the host restarts, names a process that has
    // exited, and nothing is signaled either.
    tamper("boot", serde_json::json!("f".repeat(32)));
    assert_eq!(locked.recover().unwrap(), Recovery::Exited);
    assert!(store.journal().unwrap().is_none());
    assert!(runs(recorded.pid, recorded.start_time));
}

#[test]
fn a_committed_launch_names_its_vm_for_another_build() {
    let _spawns = serialized();
    let (_directory, store) = store();
    let mut locked = store.lock(None).unwrap();
    let mut launch = locked
        .begin_launch(&LaunchId::generate().unwrap(), &runtime(), &CAPABILITY)
        .unwrap();
    let endpoint = launch.endpoint().to_owned();
    let vmm = Vmm(Some(spawn_vmm(&mut launch, &store)));
    let process = launch.record_process(vmm.0.as_ref().unwrap()).unwrap();
    // A launch records one VMM.
    assert!(launch.record_process(vmm.0.as_ref().unwrap()).is_err());
    let committed = launch.commit().unwrap();
    assert_eq!(committed.process(), process.reference());
    assert_eq!(committed.endpoint(), endpoint);
    assert_eq!(committed.runtime(), &runtime());
    assert!(store.journal().unwrap().is_none());
    assert_eq!(locked.recover().unwrap(), Recovery::Clean);
    drop(locked);

    // Another build of the orchestrator finds the record. Under the same runtime identity it
    // reattaches; under another it must stop the VM.
    let other = SandboxStore::open(store.dir()).unwrap();
    let found = other.runtime().unwrap().unwrap();
    assert_eq!(found, committed);
    assert_eq!(other.read_capability().unwrap(), CAPABILITY);
    assert_eq!(found.check_identity(&runtime()), IdentityCheck::Matches);
    let mut upgraded = runtime();
    upgraded.initramfs = artifact('4');
    assert_eq!(
        found.check_identity(&upgraded),
        IdentityCheck::Mismatched(vec![IdentityPart::Initramfs])
    );
    // The record's verified reference holds the VMM, whichever process started it.
    let reattached = Process::open(found.process())
        .unwrap()
        .expect("the recorded VMM runs");
    assert!(!reattached.has_exited().unwrap());
    drop(vmm);
    assert!(reattached.wait(Instant::now() + RECOVERY_TIMEOUT).unwrap());
    other.lock(None).unwrap().clear_runtime().unwrap();
    assert!(other.runtime().unwrap().is_none());
}

#[test]
fn an_abandoned_launch_ends_its_recorded_vmm() {
    let _spawns = serialized();
    let (_directory, store) = store();
    let mut locked = store.lock(None).unwrap();
    let mut launch = locked
        .begin_launch(&LaunchId::generate().unwrap(), &runtime(), &CAPABILITY)
        .unwrap();
    let mut vmm = Vmm(Some(spawn_vmm(&mut launch, &store)));
    let held = launch.record_process(vmm.0.as_ref().unwrap()).unwrap();
    let process = held.reference().clone();
    assert_eq!(
        launch.abandon().unwrap(),
        Recovery::Terminated(process.clone())
    );
    assert!(held.has_exited().unwrap());
    assert!(!runs(process.pid, process.start_time));
    assert!(store.journal().unwrap().is_none());
    // The caller still releases the child that it holds.
    vmm.0.take().unwrap().detach();
}

#[cfg(feature = "testing")]
#[test]
fn an_unrecorded_vmm_is_verified_through_its_endpoint_and_log() {
    let _spawns = serialized();
    let (_directory, store) = store();
    let vmm = crash_launch(&store, "after-spawn", Some(0)).unwrap();
    let mut locked = store.lock(Some(Instant::now() + RECOVERY_TIMEOUT)).unwrap();
    // Recovery waits until the fake OpenVMM serves its endpoint, which names it.
    let Recovery::Terminated(process) = recover_until_determinate(&mut locked) else {
        panic!("recovery did not terminate the VMM");
    };
    assert_eq!((process.pid, process.start_time), (vmm.pid, vmm.start_time));
    assert!(!vmm.runs());
    assert!(store.journal().unwrap().is_none());
}

/// On Linux, the endpoint is the same socket path for every launch of a sandbox, so a process that
/// serves it must also hold the launch's log. On Windows, each launch draws its endpoint's name at
/// random, so no other process serves it.
#[cfg(all(feature = "testing", target_os = "linux"))]
#[test]
fn an_endpoint_served_without_the_claimed_log_is_not_signaled() {
    let _spawns = serialized();
    // A VM of another sandbox serves an endpoint.
    let (_other_directory, other) = store();
    let mut other_locked = other.lock(None).unwrap();
    let mut other_launch = other_locked
        .begin_launch(&LaunchId::generate().unwrap(), &runtime(), &CAPABILITY)
        .unwrap();
    let served = other_launch.endpoint().to_owned();
    let fake = Vmm(Some(spawn_fake(&mut other_launch, &other, 0)));
    let fake_process = other_launch
        .record_process(fake.0.as_ref().unwrap())
        .unwrap()
        .reference()
        .clone();

    // An unrecorded VMM holds the claim of this sandbox's launch, whose journal names the other
    // sandbox's endpoint.
    let (_directory, store) = store();
    let vmm = crash_launch(&store, "after-spawn", None).unwrap();
    let path = store.dir().join(JOURNAL_NAME);
    let mut journal: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    journal["endpoint"] = served.into();
    fs::write(&path, serde_json::to_vec(&journal).unwrap()).unwrap();

    let mut locked = store.lock(None).unwrap();
    let deadline = Instant::now() + RECOVERY_TIMEOUT;
    let reason = loop {
        let Recovery::Indeterminate(reason) = locked.recover().unwrap() else {
            panic!("recovery ended a launch through a process that it did not verify");
        };
        // The fake OpenVMM may not serve its endpoint yet.
        if !reason.contains("no process serves") || Instant::now() >= deadline {
            break reason;
        }
        thread::sleep(Duration::from_millis(100));
    };
    assert!(reason.contains("does not hold its log"), "{reason}");
    assert!(runs(fake_process.pid, fake_process.start_time));
    assert!(vmm.runs());
    assert!(kill(vmm.pid, vmm.start_time).unwrap());
    assert_eq!(recover_until_determinate(&mut locked), Recovery::Exited);
    drop(fake);
}

#[test]
fn another_build_with_the_same_runtime_identity_reattaches() {
    let (_directory, store) = store();
    install(&store, "runtime.json", RUNTIME_NAME);
    let record = store.runtime().unwrap().unwrap();
    assert_eq!(record.launch().as_str(), "5d41402abc4b2a76b9719d911017c592");
    assert_eq!(record.process().pid, 4242);
    assert_eq!(record.process().start_time, 1_234_567);
    assert_eq!(
        record.process().boot.to_string(),
        "8f0e2c46a1b94d1f9e3b7a6c5d4e3f21"
    );
    assert_eq!(record.endpoint(), "/var/lib/sandboxes/example/control.sock");
    // This build speaks the contracts that the fixture's build recorded; a build that changes a
    // contract version must stop the VMs of earlier builds, and updates the fixture.
    assert_eq!(record.check_identity(&runtime()), IdentityCheck::Matches);
    // The format is pinned: this build writes what it reads.
    let written: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture("runtime.json")).unwrap()).unwrap();
    assert_eq!(serde_json::to_value(&record).unwrap(), written);
}

#[test]
fn another_runtime_identity_is_mismatched() {
    let (_directory, store) = store();
    install(&store, "runtime.json", RUNTIME_NAME);
    let record = store.runtime().unwrap().unwrap();
    let mismatched = |change: fn(&mut RuntimeIdentity), part: IdentityPart| {
        let mut current = runtime();
        change(&mut current);
        assert_eq!(
            record.check_identity(&current),
            IdentityCheck::Mismatched(vec![part])
        );
    };
    mismatched(
        |runtime| runtime.openvmm = artifact('9'),
        IdentityPart::OpenVmm,
    );
    mismatched(
        |runtime| runtime.kernel = artifact('9'),
        IdentityPart::Kernel,
    );
    mismatched(
        |runtime| runtime.initramfs = artifact('9'),
        IdentityPart::Initramfs,
    );
    mismatched(
        |runtime| runtime.boot_contract += 1,
        IdentityPart::BootContract,
    );
    // Builds before boot contract 3 recorded the microVM ABI version, 2, as the boot contract,
    // and booted guests that read a layered root's image token without a format.
    mismatched(
        |runtime| runtime.boot_contract = 2,
        IdentityPart::BootContract,
    );
    mismatched(
        |runtime| runtime.control_contract.push_str("-next"),
        IdentityPart::ControlContract,
    );
    mismatched(
        |runtime| runtime.control_session += 1,
        IdentityPart::ControlSession,
    );
}

#[test]
fn journals_of_each_launch_step_load() {
    let (_directory, store) = store();
    install(&store, "launch-prepared.json", JOURNAL_NAME);
    let prepared = store.journal().unwrap().unwrap();
    assert_eq!(prepared.state(), LaunchState::Prepared);
    assert_eq!(prepared.process(), None);
    assert_eq!(prepared.log().identity.index, 393_219);
    assert_eq!(prepared.runtime(), &runtime());
    install(&store, "launch-spawned.json", JOURNAL_NAME);
    let spawned = store.journal().unwrap().unwrap();
    assert_eq!(spawned.state(), LaunchState::Spawned);
    assert_eq!(spawned.launch(), prepared.launch());
    assert_eq!(spawned.process().unwrap().pid, 4242);
    // The journal names a process of another host's boot, which recovery never signals.
    let mut locked = store.lock(None).unwrap();
    assert_eq!(locked.recover().unwrap(), Recovery::Exited);
    assert!(store.journal().unwrap().is_none());
}

#[test]
fn unsupported_and_corrupt_records_fail_clearly() {
    let (_directory, store) = store();
    let mut locked = store.lock(None).unwrap();
    for (name, as_name, expected) in [
        (
            "runtime-cli-format-1.json",
            RUNTIME_NAME,
            "has the unsupported format 1;",
        ),
        (
            "runtime-instance-format-3.json",
            RUNTIME_NAME,
            "has the unsupported format 3;",
        ),
        (
            "launch-instance-format-3.json",
            JOURNAL_NAME,
            "has the unsupported format 3;",
        ),
        (
            "runtime-format-5.json",
            RUNTIME_NAME,
            "has the unsupported format 5;",
        ),
        ("runtime-truncated.json", RUNTIME_NAME, "is malformed"),
        ("runtime-unknown-field.json", RUNTIME_NAME, "is malformed"),
        ("launch-without-log.json", JOURNAL_NAME, "is malformed"),
    ] {
        let fixture = fixture(&format!("invalid/{name}"));
        let path = store.dir().join(as_name);
        fs::copy(&fixture, &path).unwrap();
        let read = if as_name == RUNTIME_NAME {
            store.runtime().map(drop)
        } else {
            store.journal().map(drop)
        };
        let error = read.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{name}: {error}");
        assert!(error.message().contains(expected), "{name}: {error}");
        // Nothing reads past the record or rewrites it: a launch and a recovery fail the same
        // way, and the record stays as it was.
        let launch = locked
            .begin_launch(&LaunchId::generate().unwrap(), &runtime(), &CAPABILITY)
            .map(drop);
        assert!(launch.unwrap_err().message().contains(expected), "{name}");
        if as_name == JOURNAL_NAME {
            assert!(
                locked.recover().unwrap_err().message().contains(expected),
                "{name}"
            );
        }
        assert_eq!(fs::read(&path).unwrap(), fs::read(&fixture).unwrap());
        fs::remove_file(&path).unwrap();
    }
}

/// A dropped lock is free at once, also while a process that another thread spawns still holds a
/// copy of its descriptor, as every process does from its fork until its exec when its spawn runs
/// a `pre_exec` hook: `flock` keeps a lock while any copy of its open file description remains,
/// so the lock is released before its file closes.
#[cfg(target_os = "linux")]
#[test]
fn a_dropped_lock_is_free_while_a_spawning_process_holds_its_descriptor() {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;

    use nix::libc;

    let _spawns = serialized();
    let (_directory, store) = store();
    let locked = store.lock(None).unwrap();
    let (mut forked, signal) = io::pipe().unwrap();
    let spawner = thread::spawn(move || {
        let descriptor = signal.as_raw_fd();
        let mut command = Command::new("true");
        // SAFETY: the hook makes only async-signal-safe system calls and allocates nothing, as
        // code that runs between fork and exec must.
        unsafe {
            command.pre_exec(move || {
                // The process holds a copy of every descriptor of the test now: say so, and keep
                // them for a while, through any interruption.
                let interrupted =
                    || io::Error::last_os_error().kind() == io::ErrorKind::Interrupted;
                loop {
                    match libc::write(descriptor, b"f".as_ptr().cast(), 1) {
                        1 => break,
                        -1 if interrupted() => {}
                        _ => return Err(io::Error::last_os_error()),
                    }
                }
                while libc::poll(std::ptr::null_mut(), 0, 500) < 0 {
                    if !interrupted() {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let status = command.status();
        drop(signal);
        status
    });
    let mut byte = [0];
    forked.read_exact(&mut byte).unwrap();
    drop(locked);
    let relocked = store.lock(Some(Instant::now())).map(drop);
    assert!(spawner.join().unwrap().unwrap().success());
    relocked.unwrap();
}
