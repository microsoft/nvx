//! The checked handoff of a prepared launch, through the public API of `nvx::host::sandbox` and
//! `nvx::host::supervisor`, as an orchestrator uses them: it stages and binds a distro layer and a
//! scratch image, checks them, prepares the launch, and spawns the VMM with the prepared launch's
//! arguments and allowlist.
//!
//! The VMM is this test executable, run again as a fixture. It reads the capability that
//! `nvx::host::supervisor::spawn` passes on its standard input, as OpenVMM does, reports its
//! descriptors on Linux and that it started, and, once a file named `go` appears in its working
//! directory, opens each block device that an argument names as OpenVMM opens it, a layer
//! read-only and the scratch image for reading and writing, reads each whole, writes the scratch
//! image, and reports what each held. It holds them until a file named `exit` appears in its
//! working directory, which ends it before `go` too. On Linux, the executable also runs as an
//! orchestrator fixture that closed its standard input before it checked the images.
#![cfg(all(feature = "host", any(target_os = "linux", windows)))]

use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use nvx::host::sandbox::distro::{DISTRO_POLICY_REVISION, DistroBinding};
use nvx::host::sandbox::scratch::{CLAIM_RELEASE_WAIT, ScratchBinding};
use nvx::host::sandbox::{
    LayerRole, PreparedLaunch, SandboxLaunch, SandboxLayer, SandboxSettings, SandboxUpper,
};
use nvx::host::supervisor::{self, Launched, VmmCommand};
use nvx::protocol::CAPABILITY_LEN;

/// File whose presence in its working directory marks a VMM fixture. Without it, the fixture
/// returns at once, as it does when `cargo test -- --ignored` runs it.
const VMM_MARKER: &str = "vmm-fixture";
/// File whose appearance in its working directory lets a VMM fixture open its block devices.
const GO_MARKER: &str = "go";
/// File that a VMM fixture writes once it has started, after its report of its descriptors.
const STARTED_REPORT: &str = "started";
/// File whose appearance in its working directory ends a VMM fixture.
const EXIT_MARKER: &str = "exit";
/// File in which a VMM fixture reports what its block devices held, a line for each: its role
/// and its content, escaped.
const BLOCKS_REPORT: &str = "blocks";
/// File in which a VMM fixture reports its descriptors on Linux, a line for each: its number and
/// the object that it holds.
#[cfg(target_os = "linux")]
const DESCRIPTORS_REPORT: &str = "descriptors";
/// Prefix of an argument of a VMM fixture that holds the value of a `--microvm-sandbox-block`
/// option of the prepared launch. Libtest takes the argument for a test name that matches no
/// test.
const BLOCK_PREFIX: &str = "block=";
/// File whose presence in its working directory marks a launcher fixture. Without it, the fixture
/// returns at once.
#[cfg(target_os = "linux")]
const LAUNCHER_MARKER: &str = "launcher-fixture";
/// File that a launcher fixture writes once the VMM that it launched found the checked images.
#[cfg(target_os = "linux")]
const LAUNCHED_REPORT: &str = "launched";
/// The capability of the VMMs of these tests.
const CAPABILITY: [u8; CAPABILITY_LEN] = [0x5a; CAPABILITY_LEN];
/// Longest wait for a fixture to answer.
const TIMEOUT: Duration = Duration::from_secs(60);
/// Longest life of a VMM fixture that nothing ends, so that a failed test leaves no process.
const VMM_LIFETIME: Duration = Duration::from_secs(180);
/// The UUID of the staged layer.
const UUID: &str = "11111111-1111-1111-1111-111111111111";
/// The content of the staged distro layer.
const DISTRO: &str = "distro layer\n";
/// The content of the staged scratch image.
const SCRATCH: &str = "scratch image\n";

/// The VMM fixture: reads its capability as OpenVMM does, reports its descriptors on Linux, then,
/// once [`GO_MARKER`] appears, opens and reads each block device that an argument names, writes
/// the scratch image, reports what each held, and runs until [`EXIT_MARKER`] appears.
#[test]
#[ignore = "fixture that the handoff tests run as a VMM"]
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
    write_whole(&directory.join(STARTED_REPORT), "");
    // A test that failed before it let the fixture go ends it.
    let (go, exit) = (directory.join(GO_MARKER), directory.join(EXIT_MARKER));
    let deadline = Instant::now() + VMM_LIFETIME;
    while !go.exists() {
        if exit.exists() || Instant::now() >= deadline {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let mut held = Vec::new();
    let mut report = String::new();
    for argument in env::args() {
        let Some(block) = argument.strip_prefix(BLOCK_PREFIX) else {
            continue;
        };
        let (role, rest) = block.split_once(":file:").unwrap();
        let (path, read_only) = match rest.strip_suffix(",ro") {
            Some(path) => (path, true),
            None => (rest, false),
        };
        let mut file = OpenOptions::new()
            .read(true)
            .write(!read_only)
            .open(path)
            .unwrap();
        let mut content = String::new();
        file.read_to_string(&mut content).unwrap();
        if !read_only {
            file.write_all(b"written by the VMM\n").unwrap();
        }
        report.push_str(&format!("{role} {}\n", content.escape_default()));
        held.push(file);
    }
    write_whole(&directory.join(BLOCKS_REPORT), &report);
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

/// Writes the descriptors of this VMM fixture, but for the one that lists them, to the file
/// [`DESCRIPTORS_REPORT`] in `directory`.
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

/// Returns the arguments that pass a VMM fixture the `--microvm-sandbox-block` values of
/// `prepared`.
fn block_arguments(prepared: &PreparedLaunch) -> Vec<OsString> {
    prepared
        .openvmm_arguments()
        .windows(2)
        .filter(|pair| pair[0] == "--microvm-sandbox-block")
        .map(|pair| OsString::from(format!("{BLOCK_PREFIX}{}", pair[1])))
        .collect()
}

/// Starts a VMM fixture in `vmm`'s directory with the block devices and the allowlist of
/// `prepared`, and waits until it has started.
fn start_vmm(vmm: &mut VmmDirectory, prepared: &PreparedLaunch) {
    let mut arguments = fixture_arguments("vmm_fixture");
    arguments.extend(block_arguments(prepared));
    let mut command = VmmCommand::new(env::current_exe().unwrap(), arguments, vmm.path());
    command.inherit = prepared.inherited();
    let log = File::create(vmm.path().join("vmm.log")).unwrap();
    vmm.launched = Some(supervisor::spawn(&command, &CAPABILITY, log).unwrap());
    vmm.report(STARTED_REPORT);
}

/// Returns the report of a VMM fixture whose block devices are the staged images.
fn staged_blocks() -> String {
    format!(
        "distro {}\nscratch {}\n",
        DISTRO.escape_default(),
        SCRATCH.escape_default()
    )
}

/// Tries to replace both staged images at their paths, as something that takes a path after the
/// checks would, and fails the test unless it can on Linux, where the VMM opens the checked files
/// whatever has their paths, and cannot on Windows, where the checked files hold their paths.
fn replace_images(staged: &Staged) {
    for (path, replacement) in [
        (staged.distro(), "replaced distro\n"),
        (staged.scratch(), "replaced scratch\n"),
    ] {
        let new = path.with_extension("new");
        fs::write(&new, replacement).unwrap();
        let replaced = fs::rename(&new, &path);
        assert_eq!(
            replaced.is_ok(),
            cfg!(target_os = "linux"),
            "{}: {replaced:?}",
            path.display()
        );
    }
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

/// A distro layer and a scratch image, staged in a temporary directory.
struct Staged {
    /// The temporary directory.
    directory: tempfile::TempDir,
}

impl Staged {
    /// Stages a distro layer and a scratch image.
    fn new() -> Self {
        let staged = Self {
            directory: tempfile::tempdir().unwrap(),
        };
        fs::write(staged.distro(), DISTRO).unwrap();
        fs::write(staged.scratch(), SCRATCH).unwrap();
        staged
    }

    /// Returns the path of the distro layer.
    fn distro(&self) -> PathBuf {
        self.directory.path().join("distro.erofs")
    }

    /// Returns the path of the scratch image.
    fn scratch(&self) -> PathBuf {
        self.directory.path().join("scratch.ext4")
    }

    /// Returns the launch of the distro layer over the scratch image.
    fn launch(&self) -> SandboxLaunch {
        launch_of(&self.distro(), &self.scratch())
    }
}

/// Returns the launch of the distro layer at `distro` over the scratch image at `scratch`.
fn launch_of(distro: &Path, scratch: &Path) -> SandboxLaunch {
    SandboxLaunch::new(
        vec![SandboxLayer::new(
            LayerRole::Distro,
            distro.to_str().unwrap(),
            UUID,
        )],
        SandboxUpper::scratch(scratch.to_str().unwrap()),
        SandboxSettings::default(),
    )
    .unwrap()
}

/// Sets its flag when it is dropped, also while a failure unwinds, so that a thread that runs
/// until the flag is set stops.
struct SetOnDrop<'a>(&'a AtomicBool);

impl Drop for SetOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// What [`spawn_others`] checks that no other process holds: on Linux, the objects of the checked
/// descriptors.
#[cfg(target_os = "linux")]
type Checked = PathBuf;
/// What [`spawn_others`] checks that no other process holds: on Windows, the values of the checked
/// handles, under which a process that inherited one would hold it.
#[cfg(windows)]
type Checked = usize;

/// Spawns processes until `done`, as other threads of an orchestrator do, through `std::process`,
/// and on Windows under a `SpawnLock`, as `supervisor::spawn_command` spawns them, and counts them
/// in `spawned`. Returns those that held what `checked` names, which no process but the VMM may
/// inherit.
///
/// On Linux, each process lists its own descriptors once its program runs. The spawn returns
/// before the exec has closed the descriptors that are close-on-exec, so a listing that this
/// process took then could show descriptors that never reach the program.
fn spawn_others(done: &AtomicBool, spawned: &AtomicUsize, checked: &[Checked]) -> Vec<String> {
    let mut leaks = Vec::new();
    while !done.load(Ordering::SeqCst) {
        #[cfg(target_os = "linux")]
        {
            let output = Command::new("ls")
                .args(["-l", "/proc/self/fd/"])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .unwrap();
            assert!(output.status.success());
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                if let Some((_, object)) = line.split_once(" -> ")
                    && checked.iter().any(|checked| checked.as_os_str() == object)
                {
                    leaks.push(format!("a process held {object}"));
                }
            }
        }
        #[cfg(windows)]
        {
            let mut child = supervisor::spawn_command(&mut sleeper()).unwrap();
            let pid = child.id();
            for &value in checked {
                if holds(pid, value, value) {
                    leaks.push(format!("process {pid} held handle {value:#x}"));
                }
            }
            let _ = child.kill();
            child.wait().unwrap();
        }
        spawned.fetch_add(1, Ordering::SeqCst);
    }
    leaks
}

/// Returns a command that runs for a few seconds, without standard streams.
#[cfg(windows)]
fn sleeper() -> Command {
    let mut command = Command::new("ping");
    command
        .args(["-n", "5", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// Returns the object that the descriptor `number` of this process holds, as its link in `/proc`
/// names it.
#[cfg(target_os = "linux")]
fn this_object(number: i32) -> PathBuf {
    fs::read_link(format!("/proc/self/fd/{number}")).unwrap()
}

/// Makes `pipe` inheritable, or not, as code that spawns without an allowlist does around its
/// spawns.
#[cfg(target_os = "linux")]
fn set_inheritable(pipe: &io::PipeWriter, inheritable: bool) {
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
fn set_inheritable(pipe: &io::PipeWriter, inheritable: bool) {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation};

    let flags = if inheritable { HANDLE_FLAG_INHERIT } else { 0 };
    // SAFETY: the handle is open.
    let set =
        unsafe { SetHandleInformation(pipe.as_raw_handle() as HANDLE, HANDLE_FLAG_INHERIT, flags) };
    assert_ne!(set, 0, "{}", io::Error::last_os_error());
}

/// Returns whether the handle `value` of process `pid` refers to the object of this process's
/// handle `ours`, as a handle that the process inherited from this one, under the value that it
/// has here, does.
#[cfg(windows)]
fn holds(pid: u32, value: usize, ours: usize) -> bool {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::ptr;

    use windows_sys::Win32::Foundation::{
        CompareObjectHandles, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, PROCESS_DUP_HANDLE,
    };

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
            value as HANDLE,
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
    unsafe { CompareObjectHandles(copy.as_raw_handle(), ours as HANDLE) != 0 }
}

#[test]
fn a_prepared_launch_hands_the_vmm_its_checked_images_alone() {
    let staged = Staged::new();
    let launch = staged.launch();
    // Staging binds the images, where hashing belongs; the launch checks them from their metadata
    // alone, and claims the scratch image.
    let binding = DistroBinding::verify(launch.distro()).unwrap();
    let scratch_binding = ScratchBinding::bind(launch.scratch().unwrap()).unwrap();
    let distro = binding
        .check(launch.distro(), DISTRO_POLICY_REVISION)
        .unwrap();
    let scratch = scratch_binding
        .check(
            launch.scratch().unwrap(),
            Instant::now() + CLAIM_RELEASE_WAIT,
        )
        .unwrap();
    let prepared = PreparedLaunch::new(
        launch.bound_plan(&binding).unwrap(),
        distro,
        Some(scratch),
        "",
    )
    .unwrap();

    // Something replaces both images between the checks and the VMM's opens: on Linux it can, and
    // the VMM opens the checked files anyway; on Windows the checked files keep it from doing so.
    replace_images(&staged);

    #[cfg(target_os = "linux")]
    let checked: Vec<Checked> = {
        use std::os::fd::{AsFd, AsRawFd};

        prepared
            .inherited()
            .iter()
            .map(|inherited| this_object(inherited.as_fd().as_raw_fd()))
            .collect()
    };
    #[cfg(windows)]
    let checked: Vec<Checked> = {
        use std::os::windows::io::{AsHandle, AsRawHandle};

        prepared
            .inherited()
            .iter()
            .map(|inherited| inherited.as_handle().as_raw_handle() as usize)
            .collect()
    };
    let mut vmm = VmmDirectory::new();
    let mut arguments = fixture_arguments("vmm_fixture");
    let blocks = block_arguments(&prepared);
    assert_eq!(blocks.len(), 2);
    arguments.extend(blocks);
    let mut command = VmmCommand::new(env::current_exe().unwrap(), arguments, vmm.path());
    command.inherit = prepared.inherited();
    // A descriptor or handle that this process leaves inheritable, as code that spawns without an
    // allowlist does, while another thread spawns processes.
    let (_unrelated_reader, unrelated) = io::pipe().unwrap();
    let done = AtomicBool::new(false);
    let spawned = AtomicUsize::new(0);
    let leaks = thread::scope(|scope| {
        let spawner = scope.spawn(|| spawn_others(&done, &spawned, &checked));
        // Stops the other thread when the VMM has started, or when a failure unwinds the scope,
        // which then waits for the thread.
        let stop = SetOnDrop(&done);
        let deadline = Instant::now() + TIMEOUT;
        while spawned.load(Ordering::SeqCst) == 0 {
            assert!(
                Instant::now() < deadline && !spawner.is_finished(),
                "the other thread spawned nothing"
            );
            thread::sleep(Duration::from_millis(10));
        }
        set_inheritable(&unrelated, true);
        let log = File::create(vmm.path().join("vmm.log")).unwrap();
        let launched = supervisor::spawn(&command, &CAPABILITY, log);
        set_inheritable(&unrelated, false);
        vmm.launched = Some(launched.unwrap());
        // The other thread spawns throughout the VMM's start.
        vmm.report(STARTED_REPORT);
        drop(stop);
        spawner.join().unwrap()
    });
    assert!(spawned.load(Ordering::SeqCst) > 0);
    assert!(leaks.is_empty(), "{leaks:?}");
    let pid = vmm.launched.as_ref().unwrap().id();

    // The VMM holds its standard streams and the checked files under their numbers, and nothing
    // else of this process's: on Linux, the checked descriptors, which the arguments name; on
    // Windows, the handles of the checked files, which hold the images' final paths for it, and
    // the claim's handle of the scratch image, though no argument names a handle.
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::{AsFd, AsRawFd};

        let _ = pid;
        let reported = vmm.report(DESCRIPTORS_REPORT);
        let mut expected: Vec<String> = prepared
            .inherited()
            .iter()
            .map(|inherited| {
                let number = inherited.as_fd().as_raw_fd();
                format!("{number} {}", this_object(number).display())
            })
            .collect();
        let log = fs::canonicalize(vmm.path().join("vmm.log")).unwrap();
        let lines: Vec<&str> = reported.lines().collect();
        assert_eq!(lines.len(), 5, "{reported}");
        assert!(lines[0].starts_with("0 pipe:"), "{reported}");
        assert_eq!(lines[1], format!("1 {}", log.display()));
        assert_eq!(lines[2], format!("2 {}", log.display()));
        expected.sort();
        let mut inherited: Vec<String> = lines[3..].iter().map(|line| (*line).to_owned()).collect();
        inherited.sort();
        assert_eq!(inherited, expected);
        for object in &checked {
            assert!(
                object.to_string_lossy().ends_with(" (deleted)"),
                "the checked file was not replaced at its path: {}",
                object.display()
            );
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsHandle, AsRawHandle};

        let inherited: Vec<usize> = prepared
            .inherited()
            .iter()
            .map(|inherited| inherited.as_handle().as_raw_handle() as usize)
            .collect();
        let checked = [
            prepared.distro().file().as_raw_handle() as usize,
            prepared.scratch().unwrap().file().as_raw_handle() as usize,
        ];
        assert_eq!(inherited.len(), 3);
        assert_eq!(inherited[..2], checked);
        for &value in &inherited {
            assert!(
                holds(pid, value, value),
                "the VMM does not hold handle {value:#x}"
            );
        }
        let unrelated = unrelated.as_raw_handle() as usize;
        assert!(!inherited.contains(&unrelated));
        assert!(
            !holds(pid, unrelated, unrelated),
            "the VMM holds handle {unrelated:#x}"
        );
    }

    // The VMM opens its block devices as OpenVMM does, and finds the objects that the launch
    // checked.
    File::create(vmm.path().join(GO_MARKER)).unwrap();
    let report = vmm.report(BLOCKS_REPORT);
    assert_eq!(report, staged_blocks());

    // On Windows, while the launch and the VMM hold the checked files, nothing writes the distro
    // layer, and nothing removes or renames either image, while the VMM reads and writes the
    // scratch image. Once the guest is ready, the launch releases them, its own and the VMM's
    // copies, while the VMM keeps the scratch image's claim, and what the VMM then holds, its own
    // opens and the claim's handle, shares renames.
    #[cfg(windows)]
    let moved = |path: &Path| path.with_extension("moved");
    #[cfg(windows)]
    let inherited = {
        use std::os::windows::io::{AsHandle, AsRawHandle};

        assert!(
            OpenOptions::new()
                .append(true)
                .open(staged.distro())
                .is_err()
        );
        for path in [staged.distro(), staged.scratch()] {
            assert!(fs::rename(&path, moved(&path)).is_err());
            assert!(fs::remove_file(&path).is_err());
        }
        // Each value that the VMM inherited, with a handle of this process's own to its object,
        // which outlives the release.
        prepared
            .inherited()
            .iter()
            .map(|inherited| {
                let handle = inherited.as_handle();
                (
                    handle.as_raw_handle() as usize,
                    handle.try_clone_to_owned().unwrap(),
                )
            })
            .collect::<Vec<_>>()
    };
    let process = vmm.launched.as_ref().unwrap().process().unwrap();
    prepared.release(&process).unwrap();
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;

        let held: Vec<bool> = inherited
            .iter()
            .map(|(value, object)| holds(pid, *value, object.as_raw_handle() as usize))
            .collect();
        assert_eq!(held, [false, false, true]);
        drop(inherited);
        for path in [staged.distro(), staged.scratch()] {
            rename_away_and_back(&path);
        }
    }

    // The VMM wrote the checked scratch image, whatever has its path now.
    File::create(vmm.path().join(EXIT_MARKER)).unwrap();
    let launched = vmm.launched.take().unwrap();
    assert!(
        launched
            .process()
            .unwrap()
            .wait(Instant::now() + TIMEOUT)
            .unwrap()
    );
    launched.kill_and_wait();
    let scratch = fs::read_to_string(staged.scratch()).unwrap();
    if cfg!(windows) {
        assert_eq!(scratch, format!("{SCRATCH}written by the VMM\n"));
    } else {
        assert_eq!(scratch, "replaced scratch\n");
    }
}

#[test]
fn a_vmm_keeps_the_scratch_images_claim_once_its_launcher_is_gone() {
    use nvx::ErrorKind;

    let staged = Staged::new();
    let launch = staged.launch();
    let binding = DistroBinding::verify(launch.distro()).unwrap();
    let scratch_binding = ScratchBinding::bind(launch.scratch().unwrap()).unwrap();
    let check_scratch = || {
        scratch_binding.check(
            launch.scratch().unwrap(),
            Instant::now() + CLAIM_RELEASE_WAIT,
        )
    };
    let prepared = PreparedLaunch::new(
        launch.bound_plan(&binding).unwrap(),
        binding
            .check(launch.distro(), DISTRO_POLICY_REVISION)
            .unwrap(),
        Some(check_scratch().unwrap()),
        "",
    )
    .unwrap();
    let mut vmm = VmmDirectory::new();
    start_vmm(&mut vmm, &prepared);

    // The launcher ends before the VMM has opened the images, as when it crashes, which closes
    // the checked files as dropping the prepared launch does. The VMM, which inherited the
    // scratch image's claim, keeps it: another sandbox cannot claim the image that the VMM is
    // about to write.
    drop(prepared);
    let claimed = check_scratch().unwrap_err();
    assert_eq!(claimed.kind(), ErrorKind::TimedOut, "{claimed}");

    // Something then replaces both images at their paths, before the VMM opens them: on Linux it
    // can, and on Windows the copies of the checked files that the VMM inherited keep it from
    // doing so. Either way, the VMM opens the checked images. On Linux, links that the test keeps
    // to them put them back at their paths once the VMM has opened them, for the checks below.
    #[cfg(target_os = "linux")]
    let kept: Vec<(PathBuf, PathBuf)> = [staged.distro(), staged.scratch()]
        .into_iter()
        .map(|path| {
            let kept = path.with_extension("kept");
            fs::hard_link(&path, &kept).unwrap();
            (kept, path)
        })
        .collect();
    replace_images(&staged);
    File::create(vmm.path().join(GO_MARKER)).unwrap();
    assert_eq!(vmm.report(BLOCKS_REPORT), staged_blocks());
    #[cfg(target_os = "linux")]
    for (kept, path) in kept {
        fs::rename(kept, path).unwrap();
    }

    // The claim lasts while the VMM writes the image, which changes its length here, so that a
    // binding of the written image tells the claim apart, and it ends with the VMM.
    let written = ScratchBinding::bind(launch.scratch().unwrap()).unwrap();
    assert_ne!(written, scratch_binding);
    let check_written = || {
        written.check(
            launch.scratch().unwrap(),
            Instant::now() + CLAIM_RELEASE_WAIT,
        )
    };
    let claimed = check_written().unwrap_err();
    assert_eq!(claimed.kind(), ErrorKind::TimedOut, "{claimed}");
    File::create(vmm.path().join(EXIT_MARKER)).unwrap();
    let launched = vmm.launched.take().unwrap();
    assert!(
        launched
            .process()
            .unwrap()
            .wait(Instant::now() + TIMEOUT)
            .unwrap()
    );
    launched.kill_and_wait();
    drop(check_written().unwrap());

    // On Windows, the VMM's copies of the checked files went with it too, so the images can be
    // renamed again.
    #[cfg(windows)]
    for path in [staged.distro(), staged.scratch()] {
        rename_away_and_back(&path);
    }
}

/// Renames the file at `path` away and back, which no process that holds it without sharing its
/// deletion allows, as the checked files of a launch hold it on Windows.
///
/// Waits up to [`TIMEOUT`] for a process outside the tests that has the file open to close it.
/// Once the VMM has exited, no process of the tests holds the file: they spawn their other
/// processes under the spawn lock, as `supervisor::spawn_command` does, so none inherits the
/// checked files of a launch, which are inheritable while its VMM is spawned.
#[cfg(windows)]
fn rename_away_and_back(path: &Path) {
    use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

    let moved = path.with_extension("moved");
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match fs::rename(path, &moved) {
            Ok(()) => break,
            Err(error)
                if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32)
                    && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("cannot rename {}: {error}", path.display()),
        }
    }
    fs::rename(&moved, path).unwrap();
}

/// Makes `link` a junction to the directory `target`, which needs no privilege.
#[cfg(windows)]
fn junction(link: &Path, target: &Path) {
    let mut command = Command::new("cmd");
    command
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .stdout(Stdio::null());
    let status = supervisor::spawn_command(&mut command)
        .unwrap()
        .wait()
        .unwrap();
    assert!(status.success(), "cannot link {}", link.display());
}

#[cfg(windows)]
#[test]
fn a_vmm_finds_the_checked_images_past_replaced_links_from_another_directory() {
    // The images, and decoys of them, in the target's temporary directory, which the plan names
    // through a junction, relative to this process's directory when the target is below it.
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let real = root.path().join("real");
    let decoy = root.path().join("decoy");
    for (directory, distro, scratch) in [
        (&real, DISTRO, SCRATCH),
        (&decoy, "decoy distro\n", "decoy scratch\n"),
    ] {
        fs::create_dir(directory).unwrap();
        fs::write(directory.join("distro.erofs"), distro).unwrap();
        fs::write(directory.join("scratch.ext4"), scratch).unwrap();
    }
    let link = root.path().join("link");
    junction(&link, &real);
    let named = link
        .strip_prefix(env::current_dir().unwrap())
        .map_or_else(|_| link.clone(), Path::to_path_buf);
    let launch = launch_of(&named.join("distro.erofs"), &named.join("scratch.ext4"));
    let binding = DistroBinding::verify(launch.distro()).unwrap();
    let scratch_binding = ScratchBinding::bind(launch.scratch().unwrap()).unwrap();
    let distro = binding
        .check(launch.distro(), DISTRO_POLICY_REVISION)
        .unwrap();
    let scratch = scratch_binding
        .check(
            launch.scratch().unwrap(),
            Instant::now() + CLAIM_RELEASE_WAIT,
        )
        .unwrap();
    let prepared = PreparedLaunch::new(
        launch.bound_plan(&binding).unwrap(),
        distro,
        Some(scratch),
        "",
    )
    .unwrap();

    // The arguments name the images by their final paths, which go through no link.
    let final_path = |name: &str| {
        let path = fs::canonicalize(real.join(name)).unwrap();
        path.to_str().unwrap().to_owned()
    };
    assert_eq!(
        block_arguments(&prepared),
        [
            format!(
                "{BLOCK_PREFIX}distro:file:{},ro",
                final_path("distro.erofs")
            ),
            format!("{BLOCK_PREFIX}scratch:file:{}", final_path("scratch.ext4")),
        ]
        .map(OsString::from)
    );

    // Then the link leads to the decoys, and nothing renames the directory of the images, which
    // the launch holds.
    fs::remove_dir(&link).unwrap();
    junction(&link, &decoy);
    assert!(fs::rename(&real, root.path().join("moved")).is_err());

    // The VMM, which starts in another directory, finds the checked images.
    let mut vmm = VmmDirectory::new();
    start_vmm(&mut vmm, &prepared);
    File::create(vmm.path().join(GO_MARKER)).unwrap();
    assert_eq!(vmm.report(BLOCKS_REPORT), staged_blocks());
    drop(prepared);
    drop(vmm);
}

/// The launcher fixture: an orchestrator that closes its standard input, as a daemon may, before
/// it checks the scratch image, which the check then opens as descriptor 0, under which no spawn
/// passes it. It prepares the launch, launches a VMM fixture, and writes [`LAUNCHED_REPORT`] once
/// the VMM found the checked images.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "fixture that a handoff test runs as an orchestrator without standard input"]
fn launcher_fixture() {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use nvx::ErrorKind;

    let directory = env::current_dir().unwrap();
    if !directory.join(LAUNCHER_MARKER).exists() {
        return;
    }
    let staged = Staged::new();
    let launch = staged.launch();
    let binding = DistroBinding::verify(launch.distro()).unwrap();
    let scratch_binding = ScratchBinding::bind(launch.scratch().unwrap()).unwrap();
    let distro = binding
        .check(launch.distro(), DISTRO_POLICY_REVISION)
        .unwrap();
    // SAFETY: descriptor 0, standard input, is open, and nothing in this process reads it.
    nix::unistd::close(unsafe { OwnedFd::from_raw_fd(0) }).unwrap();
    let scratch = scratch_binding
        .check(
            launch.scratch().unwrap(),
            Instant::now() + CLAIM_RELEASE_WAIT,
        )
        .unwrap();
    assert_eq!(scratch.file().as_raw_fd(), 0);
    let prepared = PreparedLaunch::new(
        launch.bound_plan(&binding).unwrap(),
        distro,
        Some(scratch),
        "",
    )
    .unwrap();

    // The image moved above the standard streams, descriptor 0 is closed again, and the image's
    // claim moved with it.
    let moved = prepared.scratch().unwrap().file().as_raw_fd();
    assert!(moved > 2, "{moved}");
    assert!(fs::read_link("/proc/self/fd/0").is_err());
    assert_eq!(
        scratch_binding
            .check(launch.scratch().unwrap(), Instant::now())
            .unwrap_err()
            .kind(),
        ErrorKind::TimedOut
    );

    // The VMM inherits the image under its new number, and finds the checked images.
    let mut vmm = VmmDirectory::new();
    start_vmm(&mut vmm, &prepared);
    let descriptors = vmm.report(DESCRIPTORS_REPORT);
    assert!(
        descriptors
            .lines()
            .any(|line| line.starts_with(&format!("{moved} "))),
        "{descriptors}"
    );
    File::create(vmm.path().join(GO_MARKER)).unwrap();
    assert_eq!(vmm.report(BLOCKS_REPORT), staged_blocks());
    drop(prepared);
    drop(vmm);
    write_whole(&directory.join(LAUNCHED_REPORT), "");
}

#[cfg(target_os = "linux")]
#[test]
fn a_launch_passes_a_checked_file_that_took_a_standard_streams_descriptor() {
    let directory = tempfile::tempdir().unwrap();
    File::create(directory.path().join(LAUNCHER_MARKER)).unwrap();
    let output = Command::new(env::current_exe().unwrap())
        .args(fixture_arguments("launcher_fixture"))
        .current_dir(directory.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(directory.path().join(LAUNCHED_REPORT).exists());
}
