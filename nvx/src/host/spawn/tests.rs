//! Tests of the spawn of a VMM: the descriptors or handles that a VMM inherits while other
//! threads spawn, its working directory on Windows, the detachment or job that keeps it running
//! or ends it, and the waits and cleanup of the child that the spawn returns.
//!
//! The processes are this test executable, run again as one of two fixtures. A VMM fixture reads
//! the capability that the spawn passes on its standard input, as OpenVMM does, reports its
//! descriptors on Linux, writes a line through each descriptor or handle that its arguments name
//! as inherited, reports that it started, and runs until a file named `exit` appears in its
//! working directory, or, when the file `exit-status` names a status, until a file named `go`
//! appears, when it exits with that status. A launcher fixture spawns a VMM fixture, prints the
//! VMM's reference as an orchestrator would record it, and runs until its standard input closes.
//! A launcher that the test kills, or whose job object the test closes, stands for an
//! orchestrator that crashed and restarts within the same boot: the test then finds the VMM
//! again, as the restarted orchestrator would.

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

use super::*;
use crate::host::process::{Opened, WaitMethod};

/// The spawn's descriptors and umask on Linux.
#[cfg(target_os = "linux")]
mod linux;

/// The VMM fixture of this test executable.
const VMM_FIXTURE: &str = "host::spawn::tests::vmm_fixture";
/// The launcher fixture of this test executable.
const LAUNCHER_FIXTURE: &str = "host::spawn::tests::launcher_fixture";
/// File whose presence in its working directory marks a VMM fixture. Without it, the fixture
/// returns at once, as it does when `cargo test -- --ignored` runs it.
const VMM_MARKER: &str = "vmm-fixture";
/// File whose appearance in its working directory ends a VMM fixture.
const EXIT_MARKER: &str = "exit";
/// File that names the status with which a VMM fixture exits once [`GO_MARKER`] appears.
const EXIT_STATUS: &str = "exit-status";
/// File whose appearance in its working directory lets a VMM fixture exit with [`EXIT_STATUS`].
const GO_MARKER: &str = "go";
/// File that a VMM fixture writes once it has started.
const STARTED_REPORT: &str = "started";
/// File in which a VMM fixture reports its descriptors on Linux, a line for each: its number and
/// the object that it holds.
#[cfg(target_os = "linux")]
const DESCRIPTORS_REPORT: &str = "descriptors";
/// Prefix of an argument of a VMM fixture that names, in decimal, a descriptor or handle that the
/// fixture inherited, and through which it writes a line. Libtest takes the argument for a test
/// name that matches no test.
const INHERITED_PREFIX: &str = "inherited=";
/// Environment variable that marks a launcher fixture, and names the working directory of its
/// VMM.
const LAUNCHER_DIR: &str = "NVX_SPAWN_TEST_VMM_DIR";
/// Environment variable that asks a launcher fixture to start its VMM outside its job object.
const LAUNCHER_BREAKAWAY: &str = "NVX_SPAWN_TEST_BREAKAWAY";
/// Marker before the reference that a launcher fixture prints. Libtest's own output, the name of
/// the fixture, precedes it on its line.
const REFERENCE_PREFIX: &str = "vmm-reference ";
/// The capability of every VMM of these tests.
const CAPABILITY: [u8; CAPABILITY_LEN] = [0x5a; CAPABILITY_LEN];
/// Longest wait for a process that is expected to exit, or for a fixture to answer.
const TIMEOUT: Duration = Duration::from_secs(60);
/// Longest life of a VMM fixture that nothing ends, so that a failed test leaves no process.
const VMM_LIFETIME: Duration = Duration::from_secs(180);
/// A wait that a running process outlasts.
const SHORT_WAIT: Duration = Duration::from_millis(100);

/// Serializes the tests that spawn VMM fixtures. On Windows, a process that one test spawns while
/// another test's handles are inheritable for a spawn of its own, as the log of a launch is,
/// could hold them, and so the claim of that log, for a while.
static SPAWNS: Mutex<()> = Mutex::new(());

/// Takes [`SPAWNS`].
fn serialized() -> MutexGuard<'static, ()> {
    SPAWNS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The VMM fixture: reads its capability as OpenVMM does, reports its descriptors on Linux,
/// writes a line through each descriptor or handle that an argument names as inherited, reports
/// that it started, then runs until a file named [`EXIT_MARKER`] appears in its working
/// directory, or exits with the status that [`EXIT_STATUS`] names once [`GO_MARKER`] appears.
#[test]
#[ignore = "fixture that the spawn tests run as a VMM"]
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
    write_whole(&directory.join(STARTED_REPORT), "");
    let status = fs::read_to_string(directory.join(EXIT_STATUS)).ok();
    let started = Instant::now();
    while !directory.join(EXIT_MARKER).exists() && started.elapsed() < VMM_LIFETIME {
        if let Some(status) = &status
            && directory.join(GO_MARKER).exists()
        {
            std::process::exit(status.parse().unwrap());
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Writes `text` to the file at `path`, which appears whole.
fn write_whole(path: &Path, text: &str) {
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, text).unwrap();
    fs::rename(&temporary, path).unwrap();
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
    write_whole(&directory.join(DESCRIPTORS_REPORT), &report);
}

/// Takes the descriptor `number`, which this VMM fixture inherited.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn inherited_file(number: usize) -> File {
    use std::os::fd::FromRawFd;

    // SAFETY: the test named a descriptor that this process inherited under that number, and
    // that nothing else in this process owns.
    unsafe { File::from_raw_fd(i32::try_from(number).unwrap()) }
}

/// Takes the handle `number`, which this VMM fixture inherited.
#[cfg(windows)]
#[allow(unsafe_code)]
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
#[ignore = "fixture that the spawn tests run as a launcher"]
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

/// Returns the command of a VMM fixture in `directory`, which it marks, outside the caller's job
/// object if `breakaway`.
fn vmm_command(directory: &Path, breakaway: bool) -> VmmCommand<'static> {
    File::create(directory.join(VMM_MARKER)).unwrap();
    VmmCommand {
        program: env::current_exe().unwrap(),
        arguments: fixture_arguments(VMM_FIXTURE),
        working_dir: directory.to_path_buf(),
        breakaway_from_job: breakaway,
        inherit: Vec::new(),
    }
}

/// Spawns a VMM fixture in `directory` as OpenVMM is spawned, outside the caller's job object if
/// `breakaway`.
fn spawn_vmm(directory: &Path, breakaway: bool) -> Launched {
    let log = File::create(directory.join("vmm.log")).unwrap();
    spawn_vm_with_mode(&vmm_command(directory, breakaway), &CAPABILITY, log, false).unwrap()
}

/// The working directory of a VMM fixture. Dropping it ends the fixture, or a VMM that ignores
/// the exit marker, and waits for it to exit, so that a failed test leaves no process behind.
struct VmmDirectory {
    /// The reference of the VMM, once the test knows it.
    reference: Option<ProcessRef>,
    /// The launched VMM, if the test holds it.
    launched: Option<Launched>,
    /// The directory, which is removed after the VMM exits.
    directory: tempfile::TempDir,
}

impl VmmDirectory {
    /// Creates an empty directory.
    fn new() -> Self {
        Self {
            reference: None,
            launched: None,
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

    /// Waits for the VMM's report `name`, and returns it.
    fn report(&self, name: &str) -> String {
        let path = self.path().join(name);
        let deadline = Instant::now() + TIMEOUT;
        while !path.exists() {
            assert!(Instant::now() < deadline, "the VMM did not report {name}");
            thread::sleep(Duration::from_millis(10));
        }
        fs::read_to_string(path).unwrap()
    }
}

impl Drop for VmmDirectory {
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
        if let Some(launched) = self.launched.take() {
            if let Ok(process) = launched.process() {
                let _ = process.wait(Instant::now() + TIMEOUT);
            }
            let _ = launched.kill_and_confirm_exit();
        }
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
    /// Starts a launcher whose VMM runs in `directory`, outside the launcher's job object if
    /// `breakaway`.
    fn start(directory: &Path, breakaway: bool) -> Self {
        let mut command = Command::new(env::current_exe().unwrap());
        command
            .args(fixture_arguments(LAUNCHER_FIXTURE))
            .env(LAUNCHER_DIR, directory);
        if breakaway {
            command.env(LAUNCHER_BREAKAWAY, "1");
        }
        command.stdin(Stdio::piped()).stdout(Stdio::piped());
        let mut child = crate::host::tests::spawn_locked(&mut command).unwrap();
        let input = child.stdin.take();
        Self { child, input }
    }

    /// Asks the fixture to start its VMM, and returns the reference that it prints, as the
    /// fixture would record it: as JSON.
    fn launch(&mut self) -> String {
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
            .expect("the fixture exited before it printed a reference")
    }

    /// Asks the fixture to start its VMM, and returns the reference that it prints.
    #[cfg(windows)]
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
#[cfg(windows)]
fn working_directory_limits_include_trailing_separators() {
    use std::os::windows::ffi::OsStrExt;

    let _spawns = serialized();
    let root = tempfile::tempdir().unwrap();
    for (units, trailing, accepted) in [
        (258, false, true),
        (259, false, false),
        (257, true, true),
        (258, true, false),
    ] {
        let name = "w".repeat(units - root.path().as_os_str().encode_wide().count() - 1);
        let directory = root.path().join(name);
        fs::create_dir_all(&directory).unwrap();
        let working = if trailing {
            directory.join("")
        } else {
            directory.clone()
        };
        let log = File::create(directory.join(format!("vmm-{units}-{trailing}.log"))).unwrap();
        let launched = spawn_vm_with_mode(&vmm_command(&working, false), &CAPABILITY, log, false);
        if accepted {
            launched.unwrap().kill_and_confirm_exit().unwrap();
        } else {
            let error = launched
                .map(|launched| launched.kill_and_confirm_exit().unwrap())
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains("258"));
        }
    }
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
    assert!(process.wait(Instant::now() + SHORT_WAIT).unwrap().is_none());
    let started = Instant::now();
    process.kill().unwrap();
    assert!(process.wait(started + TIMEOUT).unwrap().is_some());
    assert_ended(&process);
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
/// in `rounds`. Each round spawns a VMM fixture through the spawn of a VMM, which inherits a pipe
/// of its own, and a process through `std::process`, which inherits another pipe, which the round
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

        let command = VmmCommand {
            program: env::current_exe().unwrap(),
            arguments: fixture_arguments(VMM_FIXTURE),
            working_dir: directory.path().to_path_buf(),
            breakaway_from_job: false,
            inherit: vec![Inherited::new(&allowed)],
        };
        let round = rounds.load(Ordering::SeqCst);
        let log = File::create(directory.path().join(format!("{round}.log"))).unwrap();
        spawn_vm_with_mode(&command, &CAPABILITY, log, false)
            .unwrap()
            .kill_and_confirm_exit()
            .unwrap();

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
    let launched = spawn_vm_with_mode(&command, &CAPABILITY, log, false).unwrap();
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
    assert!(process.wait(Instant::now() + TIMEOUT).unwrap().is_some());
    launched.kill_and_confirm_exit().unwrap();
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

/// Returns the objects of `forbidden` that process `pid` holds a descriptor of, once its exec
/// has closed the descriptors that it did not inherit.
///
/// The kernel wakes the parent of a `vfork`, as `posix_spawn` makes one, before the child's exec
/// closes its close-on-exec descriptors, so for an instant after the spawn returns, the child's
/// table still lists every descriptor of the parent. A descriptor that the child inherited stays,
/// so this looks again until the objects are gone or a second has passed.
#[cfg(target_os = "linux")]
fn held_objects(pid: u32, forbidden: &[PathBuf]) -> Vec<PathBuf> {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let held: Vec<PathBuf> = fs::read_dir(format!("/proc/{pid}/fd"))
            .unwrap()
            .filter_map(|entry| fs::read_link(entry.unwrap().path()).ok())
            .filter(|object| forbidden.contains(object))
            .collect();
        if held.is_empty() || Instant::now() >= deadline {
            return held;
        }
        thread::sleep(Duration::from_millis(10));
    }
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
#[allow(unsafe_code)]
fn holds(pid: u32, value: usize) -> bool {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
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
            process.as_raw_handle(),
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
    unsafe { CompareObjectHandles(copy.as_raw_handle(), ours) != 0 }
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
#[allow(unsafe_code)]
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
    assert!(process.wait(Instant::now() + SHORT_WAIT).unwrap().is_none());
    let started = Instant::now();
    drop(job);
    assert!(process.wait(started + TIMEOUT).unwrap().is_some());
    assert_ended(&process);
    launcher.wait();
}

#[test]
fn launched_waits_return_and_retain_the_actual_exit_status() {
    let _spawns = serialized();
    for status in [0, 17] {
        let mut vmm = VmmDirectory::new();
        fs::write(vmm.path().join(EXIT_STATUS), status.to_string()).unwrap();
        let log = File::create(vmm.path().join("vmm.log")).unwrap();
        let command = vmm_command(vmm.path(), false);
        vmm.launched = Some(spawn_vm_with_mode(&command, &CAPABILITY, log, false).unwrap());
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
    let _spawns = serialized();
    let mut vmm = VmmDirectory::new();
    let log = File::create(vmm.path().join("vmm.log")).unwrap();
    let command = vmm_command(vmm.path(), false);
    vmm.launched = Some(spawn_vm_with_mode(&command, &CAPABILITY, log, false).unwrap());
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

/// Job objects, which Windows orchestrators run in.
#[cfg(windows)]
#[allow(unsafe_code)]
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
    pub(super) struct Job(OwnedHandle);

    impl Job {
        /// Creates a job that terminates its processes when it closes, and that lets them start
        /// processes outside it if `breakaway`.
        pub(super) fn new(breakaway: bool) -> Self {
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
        pub(super) fn assign(&self, child: &Child) {
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
