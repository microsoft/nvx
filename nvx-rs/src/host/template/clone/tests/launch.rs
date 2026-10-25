//! Tests of the launches of clones: OpenVMM's restore of the leased snapshot in place with the
//! clone's devices, activation record, and restore-ready endpoint, for which a
//! [`restore_vmm_role`] stands in, listening beside the snapshot on Linux as OpenVMM does; the
//! checks that keep a VMM from starting before the clone's binding is durable, or with a control
//! socket that OpenVMM or the host could not use; the removal of that socket; and the crashes of
//! a launch's caller, which a [`launch_role`] runs, in the launch and in the restore sequence that
//! follows it, around the clone's Ready and the start of its sandbox.

use std::cell::RefCell;
use std::env;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::*;
use crate::activation::{ReadyActivation, StoreGeneration};
use crate::host::artifacts;
use crate::host::platform::Transport;
use crate::host::process::{Process, ProcessRef};
use crate::host::store::{
    CLONE_ACTIVATION_NAME, CLONE_BINDING_NAME, CLONE_SCRATCH_NAME, JOURNAL_NAME, LaunchState,
    LockedStore, OUTCOME_NAME, RESTORE_READY_NAME, Recovery, RuntimeIdentity,
};
use crate::host::supervisor::{self, Reattached, StopOutcome, VmmCommand};
use crate::host::template::clone::launch::{Hooks, Point};
use crate::host::template::store::SNAPSHOT_ARTIFACTS;
use crate::host::template::test_support::artifact;

/// The environment variable that names the template store of a [`launch_role`].
const ROLE_STORE: &str = "NVX_CLONE_LAUNCH_TEST_STORE";
/// The environment variable that names the store generation that a [`launch_role`] leases.
const ROLE_GENERATION: &str = "NVX_CLONE_LAUNCH_TEST_GENERATION";
/// The environment variable that names the directory of the sandbox of a [`launch_role`].
const ROLE_SANDBOX: &str = "NVX_CLONE_LAUNCH_TEST_SANDBOX";
/// The environment variable that names the image of the clone of a [`launch_role`].
const ROLE_IMAGE: &str = "NVX_CLONE_LAUNCH_TEST_IMAGE";
/// The environment variable that names where a [`launch_role`] crashes: at the point `prepared`
/// or `begun` of its launch, or once its launch returned, `launched`.
const ROLE_CRASH: &str = "NVX_CLONE_LAUNCH_TEST_CRASH";
/// The environment variable that names the working directory of the VMM of a [`launch_role`].
const ROLE_VMM: &str = "NVX_CLONE_LAUNCH_TEST_VMM";
/// The start of the line in which [`launch_role`] prints the reference of the VMM that it
/// spawned, as JSON.
const REFERENCE: &str = "nvx-clone-launch-test-vmm ";
/// The start of the line that [`launch_role`] prints as it crashes.
const CRASHED: &str = "nvx-clone-launch-test-crashed ";
/// File whose presence in its working directory marks a [`restore_vmm_role`]. Without it, the
/// role returns at once, as it does when `cargo test -- --ignored` runs it.
const VMM_MARKER: &str = "vmm-role";
/// File whose appearance in its working directory ends a [`restore_vmm_role`].
const EXIT_MARKER: &str = "exit";
/// File in which [`fake_vmm`] writes the arguments that a launch gave its VMM, as JSON, for the
/// [`restore_vmm_role`] that runs instead.
const ARGUMENTS: &str = "arguments.json";
/// File in which a [`restore_vmm_role`] writes, once it has tried to send its restore-ready
/// event, the capability that it read, in hexadecimal, and whether it sent the event.
const STARTED: &str = "started";
/// File in the working directory of a [`restore_vmm_role`] whose words change how it sends its
/// restore-ready event, as [`VmmDir::plan`] describes them. Without it, the VMM sends the event at
/// once, and closes its end of the endpoint.
const EVENT_PLAN: &str = "event-plan";
/// File in which a [`restore_vmm_role`] whose plan asks it to flush its event as OpenVMM does
/// notes that the flush returned: on Windows, once the host read the event.
const FLUSHED: &str = "flushed";
/// The OpenVMM executable that the tests' launches name, which [`fake_vmm`] replaces.
pub(super) const OPENVMM: &str = "openvmm";
/// Longest wait for a VMM that is expected to start or exit, for a sandbox's lock, or for a
/// reattach to become determinate.
pub(super) const TIMEOUT: Duration = Duration::from_secs(60);
/// Longest life of a [`restore_vmm_role`] that nothing ends, so that a failed test leaves no
/// process.
const VMM_LIFETIME: Duration = Duration::from_secs(180);

/// Returns `bytes` in lowercase hexadecimal.
pub(super) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Returns the arguments that run only the role `name` of this test executable, on libtest's main
/// thread, so that libtest prints the role's name before the role's own output.
fn role(name: &str) -> Vec<OsString> {
    [
        "--exact",
        &format!("host::template::clone::tests::launch::{name}"),
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]
    .map(OsString::from)
    .to_vec()
}

/// Gives the snapshot of the generation of `fixture` its RAM and saved state beside its manifest,
/// as OpenVMM publishes them, and returns the snapshot's directory, and the content of each of
/// its files, by name.
pub(super) fn complete_snapshot(fixture: &Fixture) -> (PathBuf, Vec<(&'static str, Vec<u8>)>) {
    let snapshot = fixture.store.snapshot_dir(&fixture.generation).unwrap();
    let mut contents = Vec::new();
    for name in SNAPSHOT_ARTIFACTS {
        let path = snapshot.join(name);
        if !path.exists() {
            fs::write(&path, format!("the {name} of the template")).unwrap();
        }
        contents.push((name, fs::read(&path).unwrap()));
    }
    (snapshot, contents)
}

/// Returns the seals of the files of the snapshot in `snapshot`, by name, which change with their
/// content, their length, their modification time, and on Linux their change time, which a
/// restore checks.
fn seals(snapshot: &Path) -> Vec<(&'static str, FileSeal)> {
    SNAPSHOT_ARTIFACTS
        .map(|name| {
            let file = File::open(snapshot.join(name)).unwrap();
            (name, FileSeal::of(&file).unwrap())
        })
        .to_vec()
}

/// Returns the runtime identity that captured the template of `fixture`, under which its clones
/// launch.
pub(super) fn runtime_of(fixture: &Fixture) -> RuntimeIdentity {
    fixture
        .store
        .read_key(&fixture.generation)
        .unwrap()
        .runtime()
        .clone()
}

/// Returns whether a socket, or another file, is at `path`.
pub(super) fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Asserts that the payload of the generation of `fixture` holds no control socket of a clone's
/// VMM, and that the files of the generation's snapshot in `snapshot` have the contents
/// `contents` and the seals `sealed` still.
fn assert_untouched(
    fixture: &Fixture,
    snapshot: &Path,
    contents: &[(&str, Vec<u8>)],
    sealed: &[(&str, FileSeal)],
) {
    let payload = fixture.store.payload_dir(fixture.generation.generation());
    for entry in fs::read_dir(payload).unwrap() {
        let name = entry.unwrap().file_name();
        assert!(!name.to_string_lossy().ends_with(".sock"), "{name:?}");
    }
    for (name, content) in contents {
        assert_eq!(&fs::read(snapshot.join(name)).unwrap(), content, "{name}");
    }
    assert_eq!(seals(snapshot), sealed);
}

/// Asserts that no launch of the clone of `acquired` began or left a file: no journal, activation
/// record, restore-ready socket, or control socket of its VMM, and that the lease still reserves
/// its launch.
fn assert_unlaunched(fixture: &Fixture, acquired: &Acquired) {
    let locked = &acquired.locked;
    assert!(locked.journal().unwrap().is_none());
    for path in [
        locked.clone_activation_path(),
        locked.restore_ready_path(),
        fixture.store.clone_socket_path(&acquired.lease),
    ] {
        assert!(!exists(&path), "{}", path.display());
    }
    assert_eq!(locked.reserved(), Some(acquired.lease.launch()));
}

/// Asserts that the sandbox in `sandbox` holds none of the files that the launch of a clone
/// leaves for its release.
pub(super) fn assert_released(sandbox: &Path) {
    for name in [
        CLONE_SCRATCH_NAME,
        CLONE_ACTIVATION_NAME,
        RESTORE_READY_NAME,
    ] {
        assert!(!exists(&sandbox.join(name)), "{name}");
    }
}

/// The seam of the command of a launch that must start no VMM.
fn never_spawned(_: VmmCommand<'_>) -> VmmCommand<'_> {
    panic!("a launch that is refused made the command of a VMM")
}

/// The command that a launch made for its VMM, before [`fake_vmm`] replaced it, and what the
/// sandbox held as the VMM was about to start.
#[derive(Debug)]
pub(super) struct Made {
    /// The VMM's executable.
    program: PathBuf,
    /// The VMM's arguments.
    arguments: Vec<String>,
    /// The VMM's working directory.
    working_dir: PathBuf,
    /// How many descriptors or handles the VMM inherits.
    inherited: usize,
    /// Whether the sandbox held its clone binding.
    bound: bool,
    /// Whether the sandbox held the launch's journal.
    begun: bool,
}

/// Returns the seam of the command of a launch whose VMM is a [`restore_vmm_role`] in `vmm`: it
/// keeps the command that the launch made in `made`, writes its arguments in [`ARGUMENTS`] in
/// `vmm`, and runs the role there instead, with the descriptors or handles that the command
/// passes.
pub(super) fn fake_vmm<'a>(
    vmm: &'a Path,
    made: &'a RefCell<Option<Made>>,
) -> impl for<'h> Fn(VmmCommand<'h>) -> VmmCommand<'h> + 'a {
    move |command| {
        let arguments: Vec<String> = command
            .arguments
            .iter()
            .map(|argument| argument.to_str().unwrap().to_owned())
            .collect();
        fs::write(vmm.join(ARGUMENTS), serde_json::to_vec(&arguments).unwrap()).unwrap();
        made.replace(Some(Made {
            program: command.program.clone(),
            arguments,
            working_dir: command.working_dir.clone(),
            inherited: command.inherit.len(),
            bound: command.working_dir.join(CLONE_BINDING_NAME).is_file(),
            begun: command.working_dir.join(JOURNAL_NAME).is_file(),
        }));
        VmmCommand {
            program: env::current_exe().unwrap(),
            arguments: role("restore_vmm_role"),
            working_dir: vmm.to_owned(),
            breakaway_from_job: command.breakaway_from_job,
            inherit: command.inherit,
        }
    }
}

/// The working directory of a [`restore_vmm_role`], which it marks. Dropping it ends the VMM, and
/// waits for it to exit, so that a failed test leaves no process behind.
pub(super) struct VmmDir {
    /// The reference of the VMM, once the test knows it.
    pub(super) reference: Option<ProcessRef>,
    /// The directory, which is removed after the VMM exits.
    dir: tempfile::TempDir,
}

impl VmmDir {
    /// Creates the directory, and marks it.
    pub(super) fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(VMM_MARKER), b"").unwrap();
        Self {
            reference: None,
            dir,
        }
    }

    /// Returns the path of the directory.
    pub(super) fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Waits until the VMM has tried to send its restore-ready event, and returns what it wrote
    /// in [`STARTED`].
    pub(super) fn started(&self) -> String {
        let path = self.path().join(STARTED);
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match fs::read_to_string(&path) {
                Ok(started) => return started,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    assert!(Instant::now() < deadline, "the VMM did not start");
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("cannot read {}: {error}", path.display()),
            }
        }
    }

    /// Asks the VMM to exit.
    pub(super) fn end(&self) {
        fs::write(self.path().join(EXIT_MARKER), b"").unwrap();
    }

    /// Asks the VMM, which runs, to exit, and waits until it has. It opens the VMM's process
    /// before it asks, since a process that has exited no longer opens.
    pub(super) fn end_and_wait(&self) {
        let reference = self.reference.as_ref().expect("the test knows the VMM");
        let process = Process::open(reference)
            .unwrap()
            .expect("the VMM runs until the test ends it");
        self.end();
        assert!(process.wait(Instant::now() + TIMEOUT).unwrap());
    }

    /// Changes how the VMM sends its restore-ready event, before it starts, by the words of
    /// `plan`:
    ///
    /// - `delay:MS` sends it after MS milliseconds;
    /// - `none` sends nothing, and runs on;
    /// - `exit` sends nothing, and exits at once;
    /// - `other` sends a version of the event that OpenVMM does not send;
    /// - `partial` sends the first bytes of the event, then closes its end of the endpoint;
    /// - `flush` flushes the event as OpenVMM does, which waits on Windows until the host read
    ///   it, before the VMM writes [`STARTED`], then notes it in [`FLUSHED`].
    pub(super) fn plan(&self, plan: &str) {
        fs::write(self.path().join(EVENT_PLAN), plan).unwrap();
    }

    /// Returns whether the VMM's flush of its event returned.
    pub(super) fn flushed(&self) -> bool {
        self.path().join(FLUSHED).exists()
    }
}

impl Drop for VmmDir {
    fn drop(&mut self) {
        let _ = fs::write(self.path().join(EXIT_MARKER), b"");
        if let Some(reference) = &self.reference
            && let Ok(Some(process)) = Process::open(reference)
            && !process.wait(Instant::now() + TIMEOUT).unwrap_or(false)
        {
            let _ = process.kill();
            let _ = process.wait(Instant::now() + TIMEOUT);
        }
    }
}

/// The VMM of a clone, in a process of its own, in a directory that [`VMM_MARKER`] marks: reads
/// its capability as OpenVMM does, finds the endpoints of its control console and of the
/// restore-ready event, and the snapshot that it restores, among the arguments that its launch
/// made, which [`ARGUMENTS`] holds; holds the snapshot as OpenVMM does, which on Linux listens at
/// the control endpoint beside the snapshot, and on Windows opens the snapshot's directory and
/// files sharing nothing but reading, and serves its control endpoint's named pipe; sends the
/// restore-ready event as OpenVMM does, or as its [plan](VmmDir::plan) says; writes [`STARTED`];
/// then runs until a file named [`EXIT_MARKER`] appears, or for [`VMM_LIFETIME`] at most, and
/// removes its control socket as it exits, as OpenVMM does at a normal exit. It never answers on
/// its control endpoint.
///
/// Where OpenVMM would refuse its arguments, this VMM writes why in [`STARTED`] after its
/// capability and whether it sent its event. Where OpenVMM would abort a restore whose event it
/// cannot send, as when the launch's caller crashed, this VMM runs on, so that the test ends it as
/// it ends any VMM.
#[test]
#[ignore = "a clone's VMM that the launch tests run in a process of its own"]
fn restore_vmm_role() {
    let directory = env::current_dir().unwrap();
    if !directory.join(VMM_MARKER).exists() {
        return;
    }
    let mut capability = [0; CAPABILITY_LEN];
    std::io::stdin().read_exact(&mut capability).unwrap();
    let plan = fs::read_to_string(directory.join(EVENT_PLAN)).unwrap_or_default();
    let words: Vec<&str> = plan.split_whitespace().collect();
    if words.contains(&"exit") {
        return;
    }
    let arguments: Vec<String> =
        serde_json::from_slice(&fs::read(directory.join(ARGUMENTS)).unwrap()).unwrap();
    let (held, problem) = match hold_snapshot(&arguments) {
        Ok(held) => (Some(held), String::new()),
        Err(problem) => (None, format!(" {problem}")),
    };
    let delay = words
        .iter()
        .find_map(|word| word.strip_prefix("delay:"))
        .map_or(0, |milliseconds| milliseconds.parse().unwrap());
    thread::sleep(Duration::from_millis(delay));
    let event = if words.contains(&"other") {
        b"OPENVMM_RESTORE_READY_V2\n"
    } else if words.contains(&"partial") {
        &RESTORE_READY_EVENT[..10]
    } else {
        RESTORE_READY_EVENT
    };
    let ready = argument(&arguments, "--restore-ready-path").unwrap();
    let sent = if words.contains(&"none") {
        false
    } else {
        match send_ready(Path::new(&ready), event) {
            Ok(sink) if words.contains(&"flush") => {
                // OpenVMM flushes its event, and then closes its end of the endpoint.
                #[cfg(windows)]
                sink.sync_all().unwrap();
                drop(sink);
                fs::write(directory.join(FLUSHED), b"").unwrap();
                true
            }
            Ok(_) => true,
            Err(_) => false,
        }
    };
    let staged = directory.join(format!(".{STARTED}"));
    fs::write(&staged, format!("{} {sent}{problem}", hex(&capability))).unwrap();
    fs::rename(&staged, directory.join(STARTED)).unwrap();
    let started = Instant::now();
    while !directory.join(EXIT_MARKER).exists() && started.elapsed() < VMM_LIFETIME {
        thread::sleep(Duration::from_millis(10));
    }
    if let Some(held) = held {
        held.release();
    }
}

/// Returns the value of the option `name` among `arguments`, if they name it.
fn argument(arguments: &[String], name: &str) -> Option<String> {
    arguments
        .iter()
        .position(|argument| argument == name)
        .and_then(|at| arguments.get(at + 1).cloned())
}

/// What a [`restore_vmm_role`] holds of its restore while it runs.
struct Held {
    /// The listener at the control endpoint, on Linux, and the path at which it listens.
    #[cfg(unix)]
    listener: (std::os::unix::net::UnixListener, PathBuf),
    /// The snapshot's directory and files, open, on Windows.
    #[cfg(windows)]
    _files: Vec<File>,
    /// The instances of the control endpoint's named pipe, on Windows.
    #[cfg(windows)]
    _control: Vec<std::os::windows::io::OwnedHandle>,
}

impl Held {
    /// Lets go of the restore, as OpenVMM does at a normal exit, which removes its control socket.
    fn release(self) {
        #[cfg(unix)]
        {
            let (listener, path) = self.listener;
            drop(listener);
            fs::remove_file(path).unwrap();
        }
    }
}

/// Holds the snapshot that `arguments` restore as OpenVMM does, or returns why OpenVMM would
/// refuse them. On Linux, OpenVMM requires the parent of its control endpoint to be a directory
/// of its user with mode 0700, not a link, whose path with its links resolved is that of the
/// snapshot's parent, and listens at the endpoint's path with its parent's links resolved.
#[cfg(unix)]
fn hold_snapshot(arguments: &[String]) -> std::result::Result<Held, String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let endpoint = argument(arguments, "--microvm-control-console")
        .and_then(|console| console.strip_prefix("listen=").map(PathBuf::from))
        .ok_or("no control endpoint")?;
    let snapshot = PathBuf::from(argument(arguments, "--restore-snapshot").ok_or("no snapshot")?);
    let (Some(parent), Some(name), Some(snapshot_parent)) =
        (endpoint.parent(), endpoint.file_name(), snapshot.parent())
    else {
        return Err("the endpoint or the snapshot names no directory".to_owned());
    };
    let metadata = fs::symlink_metadata(parent).map_err(|error| error.to_string())?;
    if !metadata.is_dir()
        || metadata.uid() != crate::host::sys::effective_ids().0
        || metadata.permissions().mode() & 0o7777 != 0o700
    {
        return Err(format!(
            "the endpoint's directory {} is not one that OpenVMM takes",
            parent.display()
        ));
    }
    let canonical = fs::canonicalize(parent).map_err(|error| error.to_string())?;
    if fs::canonicalize(snapshot_parent).map_err(|error| error.to_string())? != canonical {
        return Err(format!(
            "the endpoint {} is not beside the snapshot {}",
            endpoint.display(),
            snapshot.display()
        ));
    }
    let path = canonical.join(name);
    let listener = std::os::unix::net::UnixListener::bind(&path)
        .map_err(|error| format!("cannot listen at {}: {error}", path.display()))?;
    Ok(Held {
        listener: (listener, path),
    })
}

/// Holds the snapshot that `arguments` restore as OpenVMM does, or returns why OpenVMM would
/// refuse them: on Windows, OpenVMM opens the snapshot's directory and files for reading, sharing
/// nothing but reading, so that a restore of another clone, which shares reading, reads them too,
/// while no one changes them. It also serves the named pipe of its control endpoint, as
/// [`control_pipe::serve`] does.
#[cfg(windows)]
fn hold_snapshot(arguments: &[String]) -> std::result::Result<Held, String> {
    use std::os::windows::fs::OpenOptionsExt;

    /// `FILE_SHARE_READ`.
    const SHARE_READ: u32 = 0x1;
    /// `FILE_FLAG_BACKUP_SEMANTICS`, which opens a directory.
    const BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let snapshot = PathBuf::from(argument(arguments, "--restore-snapshot").ok_or("no snapshot")?);
    let mut files = Vec::new();
    let paths = SNAPSHOT_ARTIFACTS.map(|name| (snapshot.join(name), 0));
    for (path, flags) in std::iter::once((snapshot.clone(), BACKUP_SEMANTICS)).chain(paths) {
        let file = fs::OpenOptions::new()
            .read(true)
            .share_mode(SHARE_READ)
            .custom_flags(flags)
            .open(&path)
            .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
        files.push(file);
    }
    let endpoint = argument(arguments, "--microvm-control-console")
        .and_then(|console| console.strip_prefix("listen=").map(str::to_owned))
        .ok_or("no control endpoint")?;
    let control = control_pipe::serve(&endpoint)
        .map_err(|error| format!("cannot serve the control endpoint {endpoint}: {error}"))?;
    Ok(Held {
        _files: files,
        _control: control,
    })
}

/// The named pipe of the control endpoint that a [`restore_vmm_role`] serves on Windows, as
/// OpenVMM does, through the Win32 calls that the standard library lacks.
#[cfg(windows)]
#[allow(unsafe_code)]
mod control_pipe {
    use std::io;
    use std::os::windows::io::{FromRawHandle, OwnedHandle};

    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows_sys::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    /// How many hosts may connect to the endpoint at once, one to each instance of its pipe: as
    /// many as a test's restore sequence connects, at most.
    const INSTANCES: u32 = 4;

    /// Creates the instances of the named pipe `endpoint`, `//./pipe/NAME`, each of which a host
    /// connects to before the role, which never answers, would accept it.
    pub(super) fn serve(endpoint: &str) -> io::Result<Vec<OwnedHandle>> {
        let name: Vec<u16> = endpoint
            .replace('/', "\\")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        (0..INSTANCES)
            .map(|_| {
                // SAFETY: the name is NUL-terminated, and outlives the call.
                let handle = unsafe {
                    CreateNamedPipeW(
                        name.as_ptr(),
                        PIPE_ACCESS_DUPLEX,
                        PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                        INSTANCES,
                        4096,
                        4096,
                        0,
                        std::ptr::null(),
                    )
                };
                if handle == INVALID_HANDLE_VALUE {
                    return Err(io::Error::last_os_error());
                }
                // SAFETY: CreateNamedPipeW returned a fresh handle that nothing else owns.
                Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
            })
            .collect()
    }
}

/// Sends `event` to the restore-ready endpoint at `path`, as OpenVMM sends its event: through a
/// Unix socket, or a named pipe on Windows. Returns the VMM's end of the endpoint.
fn send_ready(path: &Path, event: &[u8]) -> io::Result<File> {
    #[cfg(unix)]
    let mut sink = File::from(std::os::fd::OwnedFd::from(
        std::os::unix::net::UnixStream::connect(path)?,
    ));
    #[cfg(windows)]
    let mut sink = fs::OpenOptions::new().write(true).open(path)?;
    sink.write_all(event)?;
    Ok(sink)
}

#[test]
fn a_clone_restores_its_leased_snapshot_with_its_devices_and_activation_record() {
    let fixture = Fixture::new();
    let (snapshot, contents) = complete_snapshot(&fixture);
    let sealed = seals(&snapshot);
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let request = tmpfs_request(&image, &binding);
    let plan = fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap();
    let (key, activation) = (plan.key().clone(), plan.activation().clone());
    let capability = *plan.capability();
    let sandbox = acquired.locked.dir().to_owned();
    let lease = acquired.lease.clone();
    let mut vmm = VmmDir::new();
    let dir = vmm.path().to_owned();
    let made = RefCell::new(None);
    let command = fake_vmm(&dir, &made);
    let hooks = Hooks {
        command: &command,
        ..Hooks::LAUNCH
    };
    let restoring = plan
        .launch_with(&mut acquired, Path::new(OPENVMM), key.runtime(), hooks)
        .unwrap();
    let reference = restoring.starting().process().reference().clone();
    vmm.reference = Some(reference.clone());
    // The VMM read the plan's capability, took its arguments, and sent its event to the
    // restore-ready endpoint, which listened before the VMM started.
    assert_eq!(vmm.started(), format!("{} true", hex(&capability)));
    // The VMM started once the sandbox held the clone binding and the launch's journal, in the
    // sandbox's directory, with nothing of the caller's besides its standard streams.
    let made = made.take().unwrap();
    assert!(made.bound && made.begun, "{made:?}");
    assert_eq!(
        made.program,
        artifacts::absolute(Path::new(OPENVMM)).unwrap()
    );
    assert_eq!(made.working_dir, sandbox);
    assert_eq!(made.inherited, 0);
    // It restores the generation's snapshot in place, with the clone's devices: its image, and
    // the sparse file of its `scratch` block device. On Linux, its control console listens beside
    // the snapshot, at the socket that the lease reserved, named after the launch.
    let endpoint = restoring.starting().endpoint().to_owned();
    let ready = restoring.ready_endpoint().to_owned();
    let socket = fixture.store.clone_socket_path(&lease);
    if cfg!(target_os = "linux") {
        assert_eq!(
            socket,
            snapshot
                .parent()
                .unwrap()
                .join(format!("{}.sock", lease.launch()))
        );
        assert_eq!(endpoint, socket.to_str().unwrap());
        assert!(exists(&socket));
        assert_eq!(ready, sandbox.join(RESTORE_READY_NAME).to_str().unwrap());
    } else {
        assert!(
            endpoint.starts_with("//./pipe/openvmm-microvm-"),
            "{endpoint}"
        );
        assert!(!exists(&socket));
        assert!(ready.starts_with("//./pipe/nvx-restore-ready-"), "{ready}");
    }
    assert_eq!(restoring.starting().journal().endpoint(), endpoint);
    let distro = format!("distro:file:{},ro", shown(&image));
    let scratch = format!("scratch:file:{}", shown(&sandbox.join(CLONE_SCRATCH_NAME)));
    let (profile, processors) = (key.cpu_profile(), key.processors().to_string());
    let listen = format!("listen={endpoint}");
    let report = sandbox.join(OUTCOME_NAME);
    let expected = [
        "--machine",
        "microvm",
        "--microvm-sandbox-block",
        &distro,
        "--microvm-sandbox-block",
        &scratch,
        "--single-process",
        "--hypervisor",
        key.hypervisor().as_str(),
        "--cpu-profile",
        &profile,
        "--processors",
        &processors,
        "--restore-snapshot",
        snapshot.to_str().unwrap(),
        "--restore-memory",
        "512M",
        "--restore-ready-path",
        &ready,
        "--virtio-console",
        "none",
        "--microvm-control-console",
        &listen,
        "--microvm-control-auth-stdin",
        "--microvm-report",
        report.to_str().unwrap(),
    ];
    assert_eq!(made.arguments, expected);
    // The sandbox holds the clone's activation record, in its canonical encoding, which the
    // launch keeps.
    let recorded = fs::read(sandbox.join(CLONE_ACTIVATION_NAME)).unwrap();
    assert_eq!(recorded, activation.encode());
    assert_eq!(ActivationRecord::decode(&recorded).unwrap(), activation);
    assert_eq!(restoring.activation(), &activation);
    assert_eq!(restoring.capability(), &capability);
    assert_eq!(restoring.image().path().to_str().unwrap(), shown(&image));
    // The launch never shows its capability.
    let shown_launch = format!("{restoring:?}");
    assert!(shown_launch.starts_with("Restoring {"), "{shown_launch}");
    assert!(!shown_launch.contains("capability"), "{shown_launch}");
    assert!(
        !shown_launch.contains(&format!("{capability:?}")),
        "{shown_launch}"
    );
    // Abandoning the launch ends its VMM, which leaves its control socket, and the end of the
    // launch removes it. The release then removes the clone's files, and nothing of the
    // generation, whose snapshot the restore left as it was.
    assert_eq!(
        restoring.abandon().unwrap(),
        Recovery::Terminated(reference)
    );
    assert!(!exists(&socket));
    assert_eq!(
        fixture.store.release(&mut acquired.locked).unwrap(),
        Some(acquired.lease.clone())
    );
    assert_released(&sandbox);
    assert_untouched(&fixture, &snapshot, &contents, &sealed);
}

#[test]
fn clones_of_one_generation_restore_its_snapshot_in_place_at_once() {
    let fixture = Fixture::new();
    let (snapshot, contents) = complete_snapshot(&fixture);
    let sealed = seals(&snapshot);
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let request = tmpfs_request(&image, &binding);
    let mut clones = [fixture.acquire("first"), fixture.acquire("second")];
    let leases = clones.each_ref().map(|acquired| acquired.lease.clone());
    let mut vmms = [VmmDir::new(), VmmDir::new()];
    // Both clones plan and launch at once, each under its own sandbox's lock.
    let launched: Vec<(Restoring<'_>, Made)> = thread::scope(|scope| {
        let launches: Vec<_> = clones
            .iter_mut()
            .zip(&vmms)
            .map(|(acquired, vmm)| {
                let (fixture, request, dir) = (&fixture, &request, vmm.path().to_owned());
                scope.spawn(move || {
                    let plan = fixture.plan(acquired, request, NO_CONTENT).unwrap();
                    let runtime = plan.key().runtime().clone();
                    let made = RefCell::new(None);
                    let command = fake_vmm(&dir, &made);
                    let hooks = Hooks {
                        command: &command,
                        ..Hooks::LAUNCH
                    };
                    let restoring = plan
                        .launch_with(acquired, Path::new(OPENVMM), &runtime, hooks)
                        .unwrap();
                    (restoring, made.take().unwrap())
                })
            })
            .collect();
        launches
            .into_iter()
            .map(|launch| launch.join().unwrap())
            .collect()
    });
    for ((restoring, _), vmm) in launched.iter().zip(&mut vmms) {
        vmm.reference = Some(restoring.starting().process().reference().clone());
    }
    // Both VMMs run, restoring the one snapshot in place, each listening at its own endpoint.
    for ((restoring, made), vmm) in launched.iter().zip(&vmms) {
        let capability = hex(restoring.capability());
        assert_eq!(vmm.started(), format!("{capability} true"));
        let restored = argument(&made.arguments, "--restore-snapshot").unwrap();
        assert_eq!(restored, snapshot.to_str().unwrap());
    }
    let endpoints: Vec<&str> = launched
        .iter()
        .map(|(restoring, _)| restoring.starting().endpoint())
        .collect();
    assert_ne!(endpoints[0], endpoints[1]);
    if cfg!(target_os = "linux") {
        for (endpoint, lease) in endpoints.iter().zip(&leases) {
            let socket = fixture.store.clone_socket_path(lease);
            assert_eq!(*endpoint, socket.to_str().unwrap());
            assert!(exists(&socket));
        }
    }
    // Each VMM exits as OpenVMM does, removing its socket, and each lease is released.
    for ((restoring, _), vmm) in launched.into_iter().zip(&vmms) {
        vmm.end_and_wait();
        assert_eq!(restoring.abandon().unwrap(), Recovery::Exited);
    }
    for (acquired, lease) in clones.iter_mut().zip(leases) {
        assert_eq!(
            fixture.store.release(&mut acquired.locked).unwrap(),
            Some(lease)
        );
    }
    assert_untouched(&fixture, &snapshot, &contents, &sealed);
}

/// Launches the clone of `acquired`, whose plan `fixture` makes, with a [`restore_vmm_role`] in
/// `vmm`, and commits the launch once the VMM runs, as a clone's restore does.
fn committed(
    fixture: &Fixture,
    acquired: &mut Acquired,
    image: &(PathBuf, DistroBinding),
    vmm: &mut VmmDir,
) -> supervisor::Vmm {
    let plan = fixture
        .plan(acquired, &tmpfs_request(&image.0, &image.1), NO_CONTENT)
        .unwrap();
    let runtime = plan.key().runtime().clone();
    let dir = vmm.path().to_owned();
    let made = RefCell::new(None);
    let command = fake_vmm(&dir, &made);
    let hooks = Hooks {
        command: &command,
        ..Hooks::LAUNCH
    };
    let restoring = plan
        .launch_with(acquired, Path::new(OPENVMM), &runtime, hooks)
        .unwrap();
    vmm.reference = Some(restoring.starting().process().reference().clone());
    let capability = hex(restoring.capability());
    assert_eq!(vmm.started(), format!("{capability} true"));
    restoring.into_starting().commit().unwrap()
}

#[test]
fn the_control_socket_of_a_clone_goes_once_its_vm_has_ended_and_for_good_with_its_lease() {
    let fixture = Fixture::new();
    let (snapshot, contents) = complete_snapshot(&fixture);
    let sealed = seals(&snapshot);
    let image = fixture.image("distro.erofs", IMAGE_LEN);
    // A clone's VM whose VMM a termination ends, which leaves its control socket: the teardown
    // removes it with the runtime record.
    let mut acquired = fixture.acquire("terminated");
    let mut vmm = VmmDir::new();
    let running = committed(&fixture, &mut acquired, &image, &mut vmm);
    let socket = fixture.store.clone_socket_path(&acquired.lease);
    assert_eq!(
        running.endpoint(),
        acquired.locked.runtime().unwrap().unwrap().endpoint()
    );
    assert_eq!(exists(&socket), cfg!(target_os = "linux"));
    assert_eq!(
        running.terminate(&mut acquired.locked, TIMEOUT).unwrap(),
        StopOutcome::Forced
    );
    assert!(acquired.locked.runtime().unwrap().is_none());
    assert!(!exists(&socket));
    // A socket that comes back, as an unsynced removal can after a crash of the host, goes with
    // the release, which makes its absence durable before the lease ends.
    if cfg!(target_os = "linux") {
        fs::write(&socket, b"").unwrap();
    }
    assert_eq!(
        fixture.store.release(&mut acquired.locked).unwrap(),
        Some(acquired.lease.clone())
    );
    assert!(!exists(&socket));
    // A clone whose VMM exits on its own removes its socket itself, and a stop that finds it
    // gone ends the VM.
    let mut acquired = fixture.acquire("stopped");
    let mut vmm = VmmDir::new();
    let running = committed(&fixture, &mut acquired, &image, &mut vmm);
    let socket = fixture.store.clone_socket_path(&acquired.lease);
    let deadline = Instant::now() + TIMEOUT;
    let stopped = running
        .stop(&mut acquired.locked, deadline, None, |_, _| {
            vmm.end();
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(stopped.outcome, StopOutcome::Graceful);
    assert!(acquired.locked.runtime().unwrap().is_none());
    assert!(!exists(&socket));
    assert_eq!(
        fixture.store.release(&mut acquired.locked).unwrap(),
        Some(acquired.lease.clone())
    );
    assert_untouched(&fixture, &snapshot, &contents, &sealed);
}

#[test]
fn a_clone_of_a_template_with_a_nic_restores_its_policy_without_its_address() {
    let fixture = Fixture::with(|class, spec, physical| {
        *class = network_class();
        *spec = ManifestSpec::with_network();
        let (distro, scratch) = (
            geometry(IMAGE_LEN, physical),
            geometry(SCRATCH_LEN, physical),
        );
        set_geometries(class, spec, distro, scratch);
    });
    complete_snapshot(&fixture);
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let attached = network(NetworkAccess::Allow);
    let request = with_network(tmpfs_request(&image, &binding), Some(attached.clone()));
    let plan = fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap();
    let runtime = plan.key().runtime().clone();
    let mut vmm = VmmDir::new();
    let dir = vmm.path().to_owned();
    let made = RefCell::new(None);
    let command = fake_vmm(&dir, &made);
    let hooks = Hooks {
        command: &command,
        ..Hooks::LAUNCH
    };
    let restoring = plan
        .launch_with(&mut acquired, Path::new(OPENVMM), &runtime, hooks)
        .unwrap();
    vmm.reference = Some(restoring.starting().process().reference().clone());
    // The devices end with the arguments of the network's policy, without the guest's address,
    // which OpenVMM takes from the snapshot, and refuses beside it.
    let mut policy = attached.openvmm_arguments().unwrap();
    assert_eq!(policy[..2], ["--net", "10.0.0.2/24"]);
    policy.drain(..2);
    let arguments = made.take().unwrap().arguments;
    let devices = arguments
        .iter()
        .position(|argument| argument == "--single-process")
        .unwrap();
    assert_eq!(arguments[6..devices], policy);
    assert!(!arguments.iter().any(|argument| argument == "--net"));
}

#[test]
fn a_clone_on_a_scratch_image_passes_its_claim_to_its_vmm() {
    let fixture = Fixture::new();
    complete_snapshot(&fixture);
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let (scratch, scratch_binding) = fixture.scratch("scratch.ext4", SCRATCH_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let request = scratch_request(&image, &binding, &scratch, &scratch_binding);
    let plan = fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap();
    let runtime = plan.key().runtime().clone();
    let mut vmm = VmmDir::new();
    let dir = vmm.path().to_owned();
    let made = RefCell::new(None);
    let command = fake_vmm(&dir, &made);
    let hooks = Hooks {
        command: &command,
        ..Hooks::LAUNCH
    };
    let restoring = plan
        .launch_with(&mut acquired, Path::new(OPENVMM), &runtime, hooks)
        .unwrap();
    let reference = restoring.starting().process().reference().clone();
    vmm.reference = Some(reference.clone());
    vmm.started();
    // The VMM opens the image at its path, as the `scratch` block device, and inherits the one
    // descriptor or handle that holds the claim, which no argument names.
    let made = made.take().unwrap();
    assert_eq!(made.inherited, 1);
    let device = format!("scratch:file:{}", shown(&scratch));
    assert!(made.arguments.contains(&device), "{:?}", made.arguments);
    // On Windows, no handle that may write the image is open any more, so OpenVMM's open, which
    // shares no writing, succeeds.
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        /// `FILE_SHARE_READ | FILE_SHARE_DELETE`.
        const SHARE_READ_DELETE: u32 = 0x1 | 0x4;
        /// `FILE_FLAG_OPEN_REPARSE_POINT`.
        const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        let opened = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(SHARE_READ_DELETE)
            .custom_flags(OPEN_REPARSE_POINT)
            .open(&scratch);
        assert!(opened.is_ok(), "{opened:?}");
    }
    // The VMM holds the claim: another check finds the image claimed.
    let error = scratch_binding.check(&scratch, Instant::now()).unwrap_err();
    assert_eq!(error.code(), ErrorCode::ScratchClaimed, "{error}");
    // The claim is the VMM's alone: once the VMM has exited, the image serves another check,
    // while the launch lasts.
    vmm.end_and_wait();
    assert!(released(|| scratch_binding
        .check(&scratch, Instant::now())
        .is_ok()));
    assert_eq!(restoring.abandon().unwrap(), Recovery::Exited);
}

#[test]
fn a_clone_launches_only_as_the_launch_that_its_durable_binding_reserves() {
    let fixture = Fixture::new();
    complete_snapshot(&fixture);
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let request = tmpfs_request(&image, &binding);
    let runtime = runtime_of(&fixture);
    let openvmm = Path::new(OPENVMM);
    let hooks = Hooks {
        command: &never_spawned,
        ..Hooks::LAUNCH
    };
    // A sandbox that no longer records the clone binding starts no VMM of the clone, and keeps
    // nothing of its launch.
    let mut unbound = fixture.acquire("unbound");
    let plan = fixture.plan(&mut unbound, &request, NO_CONTENT).unwrap();
    fs::remove_file(unbound.locked.binding_path()).unwrap();
    assert_eq!(
        failure(plan.launch_with(&mut unbound, openvmm, &runtime, hooks)),
        (
            ErrorKind::Other,
            format!(
                "the sandbox in {} records no clone binding of the lease {}, which its \
                 acquisition makes durable before any VMM of the clone starts",
                unbound.locked.dir().display(),
                unbound.lease.id()
            )
        )
    );
    assert_unlaunched(&fixture, &unbound);
    // Nor does an acquisition of another lease.
    let mut first = fixture.acquire("first");
    let mut second = fixture.acquire("second");
    let plan = fixture.plan(&mut first, &request, NO_CONTENT).unwrap();
    assert_eq!(
        failure(plan.launch_with(&mut second, openvmm, &runtime, hooks)),
        (
            ErrorKind::Other,
            format!(
                "a clone is launched under the lock that acquired the lease of its plan, {}, \
                 before the launch that the lease reserves begins, but the lock of the sandbox in \
                 {} holds the lease {}, and reserves no launch {} of it",
                first.lease.id(),
                second.locked.dir().display(),
                second.lease.id(),
                first.lease.launch()
            )
        )
    );
    assert_unlaunched(&fixture, &first);
    assert_unlaunched(&fixture, &second);
    // Nor a runtime other than the one that captured the template.
    let other = RuntimeIdentity::new(artifact('d'), artifact('e'), artifact('f'));
    assert_ne!(other, runtime);
    let plan = fixture.plan(&mut first, &request, NO_CONTENT).unwrap();
    let (kind, message) = failure(plan.launch_with(&mut first, openvmm, &other, hooks));
    assert_eq!(kind, ErrorKind::Unsupported, "{message}");
    assert!(
        message.starts_with(&format!(
            "the template's generation {} restores under the runtime that captured it",
            fixture.generation.generation()
        )),
        "{message}"
    );
    assert_unlaunched(&fixture, &first);
    // On Linux, a sandbox whose restore-ready socket's path would not fit a Unix socket's
    // address is refused.
    #[cfg(target_os = "linux")]
    {
        let mut deep = fixture.acquire(&"d".repeat(100));
        let plan = fixture.plan(&mut deep, &request, NO_CONTENT).unwrap();
        assert_eq!(
            failure(plan.launch_with(&mut deep, openvmm, &runtime, hooks)),
            (
                ErrorKind::Unsupported,
                too_long(&deep.locked.restore_ready_path())
            )
        );
        assert_unlaunched(&fixture, &deep);
    }
    // The acquisition launches once its plan is its own.
    let plan = fixture.plan(&mut second, &request, NO_CONTENT).unwrap();
    let mut vmm = VmmDir::new();
    let dir = vmm.path().to_owned();
    let made = RefCell::new(None);
    let command = fake_vmm(&dir, &made);
    let hooks = Hooks {
        command: &command,
        ..Hooks::LAUNCH
    };
    let restoring = plan
        .launch_with(&mut second, openvmm, &runtime, hooks)
        .unwrap();
    vmm.reference = Some(restoring.starting().process().reference().clone());
}

/// Returns the message of the refusal of a clone's launch whose socket at `path` would not fit a
/// Unix socket's address.
#[cfg(target_os = "linux")]
fn too_long(path: &Path) -> String {
    format!(
        "the path of the socket {} of a clone's launch must be shorter than 108 bytes, as a Unix \
         socket's address requires",
        path.display()
    )
}

#[cfg(target_os = "linux")]
#[test]
fn a_clone_whose_control_socket_openvmm_or_the_host_cannot_use_starts_nothing() {
    use std::os::unix::fs::PermissionsExt;

    /// Returns a directory of 60 characters in `directory`, private to the current user.
    fn deep(directory: &Path) -> PathBuf {
        let deep = directory.join("d".repeat(60));
        assert_eq!(platform::create_owner_only_dir(&deep).unwrap(), None);
        deep
    }

    let hooks = Hooks {
        command: &never_spawned,
        ..Hooks::LAUNCH
    };
    let refused = |fixture: &Fixture, sandbox: &str| {
        complete_snapshot(fixture);
        let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
        let mut acquired = fixture.acquire(sandbox);
        let plan = fixture
            .plan(&mut acquired, &tmpfs_request(&image, &binding), NO_CONTENT)
            .unwrap();
        let runtime = runtime_of(fixture);
        let refusal = failure(plan.launch_with(&mut acquired, Path::new(OPENVMM), &runtime, hooks));
        assert_unlaunched(fixture, &acquired);
        (refusal, fixture.store.clone_socket_path(&acquired.lease))
    };
    // A store whose path leaves no room for the socket of a clone's VMM in a payload.
    let fixture = Fixture::in_store(|directory| deep(directory).join("templates"), |_, _, _| {});
    let ((kind, message), socket) = refused(&fixture, "sandbox");
    assert!(socket.as_os_str().len() >= 108);
    assert_eq!((kind, message), (ErrorKind::Unsupported, too_long(&socket)));
    // A store whose path is short, but long once its links are resolved, at which OpenVMM listens.
    let fixture = Fixture::in_store(
        |directory| {
            let link = directory.join("l");
            std::os::unix::fs::symlink(deep(directory), &link).unwrap();
            link.join("templates")
        },
        |_, _, _| {},
    );
    let ((kind, message), socket) = refused(&fixture, "sandbox");
    assert!(socket.as_os_str().len() < 108, "{}", socket.display());
    let resolved = filesystem::canonicalize(socket.parent().unwrap())
        .unwrap()
        .join(socket.file_name().unwrap());
    assert_eq!(
        (kind, message),
        (ErrorKind::Unsupported, too_long(&resolved))
    );
    // A payload private to the current user, as a plan requires, but whose mode is not 0700, as
    // OpenVMM requires of the directory of its control socket.
    let fixture = Fixture::new();
    let payload = fixture.store.payload_dir(fixture.generation.generation());
    fs::set_permissions(&payload, fs::Permissions::from_mode(0o500)).unwrap();
    let ((kind, message), _) = refused(&fixture, "sandbox");
    fs::set_permissions(&payload, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        (kind, message),
        (
            ErrorKind::Io,
            format!(
                "the payload of the generation {} has mode 500, not the mode 700 that the \
                 directory of a control socket must have",
                payload.display()
            )
        )
    );
}

/// A clone's launch in a process of its own, which [`run_launch_role`] runs: it leases the store
/// generation [`ROLE_GENERATION`] of the store in [`ROLE_STORE`] to the sandbox in
/// [`ROLE_SANDBOX`], plans its clone of the image in [`ROLE_IMAGE`] in RAM, and launches it with a
/// [`restore_vmm_role`] in [`ROLE_VMM`] as its VMM; and it exits without unwinding where
/// [`ROLE_CRASH`] says, as a crash would, once it has printed the reference of the VMM that it
/// spawned, if any.
#[test]
#[ignore = "a clone's launch that the launch tests run in a process of its own"]
fn launch_role() {
    let (Some(dir), Ok(generation), Some(sandbox), Some(image), Ok(step), Some(vmm)) = (
        env::var_os(ROLE_STORE),
        env::var(ROLE_GENERATION),
        env::var_os(ROLE_SANDBOX).map(PathBuf::from),
        env::var_os(ROLE_IMAGE).map(PathBuf::from),
        env::var(ROLE_CRASH),
        env::var_os(ROLE_VMM).map(PathBuf::from),
    ) else {
        return;
    };
    let store = Store::open(dir).unwrap();
    let generation = StoreGeneration::new(generation.parse().unwrap());
    let indexed = store.index().unwrap().get(generation).unwrap().clone();
    let mut acquired = store
        .acquire(generation, sandbox, &files(), &after(&indexed, 1), None)
        .unwrap();
    let binding = DistroBinding::import(&layer(&image), PolicyVerdict::ACCEPTED).unwrap();
    let plan = ClonePlan::new_with(
        &store,
        &mut acquired,
        &tmpfs_request(&image, &binding),
        Instant::now() + CLAIM_RELEASE_WAIT,
        NO_CONTENT,
    )
    .unwrap();
    let runtime = plan.key().runtime().clone();
    let made = RefCell::new(None);
    let command = fake_vmm(&vmm, &made);
    let interrupt = |point: Point| -> Result<()> {
        if format!("{point:?}").to_lowercase() == step {
            crash(&step);
        }
        Ok(())
    };
    let hooks = Hooks {
        command: &command,
        interrupt: &interrupt,
    };
    let restoring = plan
        .launch_with(&mut acquired, Path::new(OPENVMM), &runtime, hooks)
        .unwrap();
    println!(
        "\n{REFERENCE}{}",
        serde_json::to_string(restoring.starting().process().reference()).unwrap()
    );
    if step == "launched" {
        crash(&step);
    }
    // The restore sequence, through a framing that crashes within the Activate or the start.
    let mut control = RoleControl { step: &step };
    let activated = restoring
        .activate(&mut control, RestoreDeadlines::default())
        .unwrap();
    if step == "activated" {
        crash(&step);
    }
    let started = activated.start(&mut control).unwrap();
    if step == "started" {
        crash(&step);
    }
    let committed = started.commit().unwrap();
    if step == "committed" {
        crash(&step);
    }
    drop(committed);
    panic!("the clone's launch did not crash at {step}");
}

/// The adapter's framing of a [`launch_role`], whose clone answers as the record asks, and which
/// crashes as it awaits the clone's Ready, at the step `activating`, or the start of its sandbox,
/// at the step `starting`.
struct RoleControl<'a> {
    /// The step at which the role crashes.
    step: &'a str,
}

impl CloneControl for RoleControl<'_> {
    type Session = Box<dyn Transport>;
    type Error = io::Error;

    fn attach(
        &mut self,
        transport: Box<dyn Transport>,
        _capability: &[u8; CAPABILITY_LEN],
        _deadline: Instant,
    ) -> io::Result<Box<dyn Transport>> {
        Ok(transport)
    }

    fn activate(
        &mut self,
        _session: &mut Box<dyn Transport>,
        record: &[u8],
        _deadline: Instant,
    ) -> io::Result<Option<ReadyActivation>> {
        if self.step == "activating" {
            crash(self.step);
        }
        let record = ActivationRecord::decode(record).unwrap();
        Ok(Some(ReadyActivation::of(&record)))
    }

    fn start(&mut self, _session: &mut Box<dyn Transport>, _deadline: Instant) -> io::Result<()> {
        if self.step == "starting" {
            crash(self.step);
        }
        Ok(())
    }

    fn lost(&self, _error: &io::Error) -> bool {
        false
    }
}

/// Ends this process at once, once it has printed that it crashes at `step`, without unwinding
/// or running destructors, as a crash would.
fn crash(step: &str) -> ! {
    println!("\n{CRASHED}{step}");
    std::io::stdout().flush().unwrap();
    std::process::exit(0)
}

/// Runs [`launch_role`] for the generation of `fixture`, the sandbox in `sandbox`, the image
/// `image`, and a VMM in `vmm`, until it crashes at `step`, and returns the reference of the VMM
/// that it spawned, if it spawned one.
fn run_launch_role(
    step: &str,
    fixture: &Fixture,
    sandbox: &Path,
    image: &Path,
    vmm: &Path,
) -> Option<ProcessRef> {
    let output = Command::new(env::current_exe().unwrap())
        .args(role("launch_role"))
        .env(ROLE_STORE, fixture.store.dir())
        .env(ROLE_GENERATION, fixture.generation.generation().to_string())
        .env(ROLE_SANDBOX, sandbox)
        .env(ROLE_IMAGE, image)
        .env(ROLE_CRASH, step)
        .env(ROLE_VMM, vmm)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let crashed = format!("{CRASHED}{step}");
    assert!(
        output.status.success() && stdout.lines().any(|line| line == crashed),
        "{}: {stdout}{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(REFERENCE))
        .map(|reference| serde_json::from_str(reference).unwrap())
}

/// A clone whose launch's caller, a [`launch_role`], crashed, in a fresh fixture: the VMM's
/// working directory, the sandbox, and the lease that the sandbox holds.
struct Crashed {
    /// The working directory of the clone's VMM, which ends the VMM as it drops, before the rest.
    vmm: VmmDir,
    /// The template store, its generation, and the clone's image.
    fixture: Fixture,
    /// The sandbox's directory.
    sandbox: PathBuf,
    /// The lease that the sandbox holds.
    lease: Lease,
    /// The runtime identity under which the clone launched.
    runtime: RuntimeIdentity,
}

impl Crashed {
    /// Leases a fresh fixture's generation to a new sandbox, whose clone a [`launch_role`]
    /// launches until it crashes at `step`.
    fn at(step: &str) -> Self {
        let fixture = Fixture::new();
        complete_snapshot(&fixture);
        let (image, _) = fixture.image("distro.erofs", IMAGE_LEN);
        let sandbox = fixture.provision("sandbox");
        let mut vmm = VmmDir::new();
        if !matches!(step, "prepared" | "begun" | "launched") {
            // The restore sequence reads the event, whose flush waits for that on Windows.
            vmm.plan("flush");
        }
        vmm.reference = run_launch_role(step, &fixture, &sandbox, &image, vmm.path());
        let lease = Lease::of(&SandboxStore::open(&sandbox).unwrap())
            .unwrap()
            .expect("the crash kept the clone's lease");
        let runtime = runtime_of(&fixture);
        Self {
            vmm,
            fixture,
            sandbox,
            lease,
            runtime,
        }
    }

    /// Takes the lock of the clone's sandbox, as the restarted caller does.
    fn lock(&self) -> LockedStore {
        SandboxStore::lock_existing(&self.sandbox, &files(), Some(Instant::now() + TIMEOUT))
            .unwrap()
    }

    /// Checks that the sandbox holds the clone's activation record, which names the restore of
    /// the leased generation, and, on Linux, the restore-ready socket.
    fn kept_its_files(&self, locked: &LockedStore) {
        let recorded = fs::read(locked.clone_activation_path()).unwrap();
        let record = ActivationRecord::decode(&recorded).unwrap();
        assert_eq!(record.restore().unwrap().template, self.lease.template());
        if cfg!(target_os = "linux") {
            assert!(exists(&locked.restore_ready_path()));
        }
    }

    /// Returns the path of the control socket of the clone's VMM, beside the snapshot that it
    /// restores.
    fn socket(&self) -> PathBuf {
        self.fixture.store.clone_socket_path(&self.lease)
    }

    /// Reattaches to the VM of the sandbox of `locked` until the result is determinate, as a
    /// caller retries an indeterminate reattach, and returns it.
    fn reattach(&self, locked: &mut LockedStore) -> Reattached {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match supervisor::reattach(locked, &self.runtime).unwrap() {
                Reattached::Indeterminate(reason) if Instant::now() < deadline => {
                    eprintln!("reattach is indeterminate: {reason}");
                    thread::sleep(Duration::from_millis(100));
                }
                reattached => return reattached,
            }
        }
    }

    /// Checks that the store refuses to release the lease of the sandbox of `locked`, whose
    /// launch has not ended, and that the sandbox keeps it.
    fn refuses_release(&self, locked: &mut LockedStore) {
        let error = self.fixture.store.release(locked).unwrap_err();
        assert_eq!(error.code(), ErrorCode::SandboxRunning, "{error}");
        assert_eq!(Lease::of(locked).unwrap(), Some(self.lease.clone()));
    }

    /// Checks that the store releases the lease of the sandbox of `locked`, which removes the
    /// clone's files, and nothing of the generation, whose payload holds no socket of the clone.
    fn releases(&self, locked: &mut LockedStore) {
        assert_eq!(
            self.fixture.store.release(locked).unwrap(),
            Some(self.lease.clone())
        );
        assert_released(&self.sandbox);
        assert!(!exists(&self.socket()));
        let snapshot = self.fixture.store.snapshot_dir(&self.fixture.generation);
        for name in SNAPSHOT_ARTIFACTS {
            assert!(snapshot.as_ref().unwrap().join(name).is_file(), "{name}");
        }
    }
}

#[test]
fn a_launch_whose_caller_crashes_before_it_begins_leaves_files_that_the_release_removes() {
    let crashed = Crashed::at("prepared");
    assert_eq!(crashed.vmm.reference, None);
    let mut locked = crashed.lock();
    // The launch never began: the sandbox records no launch, and keeps the clone's files.
    assert!(locked.journal().unwrap().is_none());
    crashed.kept_its_files(&locked);
    match crashed.reattach(&mut locked) {
        Reattached::Exited {
            record: None,
            recovery: Recovery::Clean,
        } => {}
        other => panic!("unexpected reattach: {other:?}"),
    }
    crashed.releases(&mut locked);
}

#[test]
fn a_launch_whose_caller_crashes_once_it_began_ends_as_reattach_recovers_it() {
    let crashed = Crashed::at("begun");
    assert_eq!(crashed.vmm.reference, None);
    let mut locked = crashed.lock();
    // The journal names the launch that the lease reserved, which spawned nothing, and keeps the
    // lease until a reattach ends it.
    let journal = locked.journal().unwrap().unwrap();
    assert_eq!(journal.launch(), crashed.lease.launch());
    assert_eq!(journal.state(), LaunchState::Prepared);
    crashed.kept_its_files(&locked);
    crashed.refuses_release(&mut locked);
    match crashed.reattach(&mut locked) {
        Reattached::Exited {
            record: None,
            recovery: Recovery::Exited,
        } => {}
        other => panic!("unexpected reattach: {other:?}"),
    }
    crashed.releases(&mut locked);
}

#[test]
fn a_launch_whose_caller_crashes_once_its_vmm_started_ends_the_vmm_as_reattach_recovers_it() {
    let crashed = Crashed::at("launched");
    let reference = crashed.vmm.reference.clone().unwrap();
    let process = Process::open(&reference).unwrap().unwrap();
    // The VMM listens beside the snapshot on Linux, as it has once it tried to send its event.
    crashed.vmm.started();
    assert_eq!(exists(&crashed.socket()), cfg!(target_os = "linux"));
    let mut locked = crashed.lock();
    // The journal records the VMM, which never committed: reattach ends it through its verified
    // reference, and the lease lasts until then. The VMM, which the termination ends, leaves its
    // control socket, which the end of the launch removes.
    let journal = locked.journal().unwrap().unwrap();
    assert_eq!(journal.launch(), crashed.lease.launch());
    assert_eq!(journal.state(), LaunchState::Spawned);
    assert_eq!(journal.process(), Some(&reference));
    if cfg!(target_os = "linux") {
        assert_eq!(journal.endpoint(), crashed.socket().to_str().unwrap());
    }
    crashed.kept_its_files(&locked);
    crashed.refuses_release(&mut locked);
    match crashed.reattach(&mut locked) {
        Reattached::Exited {
            record: None,
            recovery: Recovery::Terminated(terminated),
        } => assert_eq!(terminated, reference),
        other => panic!("unexpected reattach: {other:?}"),
    }
    assert!(process.wait(Instant::now() + TIMEOUT).unwrap());
    assert!(!exists(&crashed.socket()));
    crashed.releases(&mut locked);
}

#[test]
fn a_restore_whose_caller_crashes_before_its_commit_ends_its_vmm_as_reattach_recovers_it() {
    // The caller crashes as it awaits the clone's Ready, once the clone is ready, as it awaits the
    // start of the clone's sandbox, and once the sandbox started: each time, the launch has not
    // committed, and its journal names the VMM.
    for step in ["activating", "activated", "starting", "started"] {
        let crashed = Crashed::at(step);
        let reference = crashed.vmm.reference.clone().unwrap();
        let process = Process::open(&reference).unwrap().unwrap();
        // The caller read the VMM's restore-ready event, whose flush waited for that on Windows.
        assert!(crashed.vmm.started().ends_with(" true"), "{step}");
        assert!(crashed.vmm.flushed(), "{step}");
        let mut locked = crashed.lock();
        let journal = locked.journal().unwrap().unwrap();
        assert_eq!(journal.launch(), crashed.lease.launch(), "{step}");
        assert_eq!(journal.state(), LaunchState::Spawned, "{step}");
        assert_eq!(journal.process(), Some(&reference), "{step}");
        assert!(locked.runtime().unwrap().is_none(), "{step}");
        crashed.kept_its_files(&locked);
        // The lease lasts until reattach ended the VMM, through its verified reference.
        crashed.refuses_release(&mut locked);
        match crashed.reattach(&mut locked) {
            Reattached::Exited {
                record: None,
                recovery: Recovery::Terminated(terminated),
            } => assert_eq!(terminated, reference, "{step}"),
            other => panic!("unexpected reattach at {step}: {other:?}"),
        }
        assert!(process.wait(Instant::now() + TIMEOUT).unwrap(), "{step}");
        assert!(!exists(&crashed.socket()), "{step}");
        crashed.releases(&mut locked);
    }
}

#[test]
fn a_restore_whose_caller_crashes_once_it_committed_keeps_its_lease_while_its_vm_runs() {
    let crashed = Crashed::at("committed");
    let reference = crashed.vmm.reference.clone().unwrap();
    let mut locked = crashed.lock();
    // The runtime record names the VM, which reattach holds again, and the lease lasts as long as
    // the VM runs.
    assert!(locked.journal().unwrap().is_none());
    let record = locked.runtime().unwrap().unwrap();
    assert_eq!(record.process(), &reference);
    crashed.kept_its_files(&locked);
    let vmm = match crashed.reattach(&mut locked) {
        Reattached::Live(vmm) => vmm,
        other => panic!("unexpected reattach: {other:?}"),
    };
    assert_eq!(vmm.process().reference(), &reference);
    crashed.refuses_release(&mut locked);
    // Once a termination verified the VM's exit, the release ends the lease.
    assert_eq!(
        vmm.terminate(&mut locked, TIMEOUT).unwrap(),
        StopOutcome::Forced
    );
    assert!(!exists(&crashed.socket()));
    crashed.releases(&mut locked);
}
