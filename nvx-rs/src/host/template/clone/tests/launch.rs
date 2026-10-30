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
use crate::activation::{ReadyActivation, StoreGeneration, TransactionId};
use crate::host::artifacts;
use crate::host::platform::Transport;
use crate::host::process::{BootId, Process, ProcessRef};
use crate::host::store::files::{sync_dir, write_private};
use crate::host::store::{
    CLONE_ACTIVATION_NAME, CLONE_BINDING_NAME, CLONE_FAILURE_NAME, CLONE_PROGRESS_NAME,
    JOURNAL_NAME, LaunchState, LockedStore, OUTCOME_NAME, RESTORE_READY_NAME, Recovery,
    RuntimeIdentity, RuntimeRecord,
};
use crate::host::supervisor::{self, Reattached, StopOutcome, Vmm, VmmCommand};
use crate::host::template::Retirement;
use crate::host::template::clone::launch::{Hooks, Point};
use crate::host::template::clone::reattach::{reattach_clone_syncing, reattach_clone_with};
use crate::host::template::clone::records::{read_failure, read_progress, write_failure};
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
/// or `begun` of its launch, once its launch returned, `launched`, at the steps `activating`,
/// `activated`, `starting`, `started`, and `committed` of its restore sequence, and once its
/// commit left the launch's journal beside the runtime record, `superseded`.
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
/// How long a [`restore_vmm_role`] whose plan closes the event's endpoint waits once it connected
/// the endpoint, and once it closed it, before it reports its error.
const CLOSE_PAUSE: Duration = Duration::from_millis(300);

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
        CLONE_ACTIVATION_NAME,
        CLONE_PROGRESS_NAME,
        CLONE_FAILURE_NAME,
        RESTORE_READY_NAME,
    ] {
        assert!(!exists(&sandbox.join(name)), "{name}");
    }
}

/// Checks that `failure`, of the clone of `lease` in the sandbox of `locked`, failed at `stage`,
/// once the clone got as far as `progress`, with `evidence`, which attributes it, and the teardown
/// `teardown`, and that the sandbox records it, with the clone's lease and the transaction of its
/// restore.
pub(super) fn assert_recorded(
    failure: &CloneFailure,
    lease: &Lease,
    locked: &LockedStore,
    (stage, progress): (Stage, Progress),
    evidence: Evidence,
    teardown: Teardown,
) {
    let record = failure.record();
    assert_eq!(
        (
            record.stage,
            record.progress,
            record.evidence,
            &record.teardown
        ),
        (stage, progress, evidence, &teardown),
        "{failure}"
    );
    assert_eq!(failure.outcome(), evidence.outcome(), "{failure}");
    assert_eq!(
        (record.store, record.lease, record.template, &record.launch),
        (lease.store(), lease.id(), lease.template(), lease.launch())
    );
    let activation = fs::read(locked.clone_activation_path()).unwrap();
    let activation = ActivationRecord::decode(&activation).unwrap();
    let transaction = activation.restore().unwrap().transaction;
    assert_eq!(record.transaction, Some(transaction));
    assert_eq!(record.code, failure.code());
    assert!(record.message.starts_with(failure.message()), "{failure}");
    let records = CloneRecords::read(locked).unwrap().unwrap();
    assert_eq!(records.failure.as_ref(), Some(record));
    assert_eq!(&records.lease, lease);
}

/// Returns what a reattach found, as test messages say it.
pub(super) fn describe<S>(reattached: &ReattachedClone<S>) -> String {
    match reattached {
        ReattachedClone::Live { vmm, .. } => format!("the live VM {:?}", vmm.record()),
        ReattachedClone::Ended { progress, report } => {
            format!("a clone that ended at its step {progress}, with the report {report:?}")
        }
        ReattachedClone::Failed(failure) => format!("the failure {:?}", failure.record()),
        ReattachedClone::Indeterminate(reason) => format!("an indeterminate VM: {reason}"),
    }
}

/// Returns the failure that a reattach found, and fails on anything else.
pub(super) fn failed<S>(reattached: ReattachedClone<S>) -> CloneFailure {
    match reattached {
        ReattachedClone::Failed(failure) => failure,
        other => panic!("the reattach found {}", describe(&other)),
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

    /// Waits until the VMM, which exits on its own, as its [plan](Self::plan) asks, has exited.
    pub(super) fn wait_exit(&self) {
        let reference = self.reference.as_ref().expect("the test knows the VMM");
        if let Some(process) = Process::open(reference).unwrap() {
            assert!(process.wait(Instant::now() + TIMEOUT).unwrap());
        }
    }

    /// Changes how the VMM sends its restore-ready event, before it starts, by the words of
    /// `plan`:
    ///
    /// - `delay:MS` sends it after MS milliseconds;
    /// - `none` sends nothing, and runs on;
    /// - `exit` sends nothing, and exits at once;
    /// - `close` connects to the event's endpoint, and closes it without a byte of the event, then
    ///   exits, [`CLOSE_PAUSE`] after each, as OpenVMM does once the launch of its VM worker
    ///   fails;
    /// - `fatal:CODE`, with `exit` or `close`, writes the line with which OpenVMM reports a fatal
    ///   error of the time ABI code `CODE`, and the error's chain, on its standard error, after a
    ///   warning that names another code, before it exits;
    /// - `forward:PORT`, with `exit` or `close`, writes the line with which OpenVMM reports a fatal
    ///   error without a code, and the error's chain, whose second cause names the forward of host
    ///   loopback's TCP port `PORT` that OpenVMM cannot bind, on its standard error, after a
    ///   warning that names that forward too, before it exits;
    /// - `other` sends a version of the event that OpenVMM does not send;
    /// - `partial` sends the first bytes of the event, then closes its end of the endpoint;
    /// - `flush` flushes the event as OpenVMM does, which waits on Windows until the host read
    ///   it, before the VMM writes [`STARTED`], then notes it in [`FLUSHED`];
    /// - `report:CATEGORY:STATUS` writes OpenVMM's outcome report of a VM whose outcome is of the
    ///   category `CATEGORY`, with the status `STATUS`, as the VMM exits.
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
/// then runs until a file named [`EXIT_MARKER`] appears, or for [`VMM_LIFETIME`] at most, writes
/// the outcome report that its plan asks for, and removes its control socket as it exits, as
/// OpenVMM does at a normal exit. It never answers on its control endpoint.
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
    let arguments: Vec<String> =
        serde_json::from_slice(&fs::read(directory.join(ARGUMENTS)).unwrap()).unwrap();
    if words.contains(&"exit") || words.contains(&"close") {
        if words.contains(&"close") {
            // As OpenVMM does once the launch of its VM worker fails: it connected the event's
            // endpoint before it launched the worker, closes the endpoint as the launch fails,
            // tears down, and only then reports the error as it exits.
            let ready = argument(&arguments, "--restore-ready-path").unwrap();
            let sink = send_ready(Path::new(&ready), b"");
            thread::sleep(CLOSE_PAUSE);
            drop(sink);
            thread::sleep(CLOSE_PAUSE);
        }
        if let Some(code) = words.iter().find_map(|word| word.strip_prefix("fatal:")) {
            // OpenVMM's warnings may name a time ABI code too; its fatal error leads with the
            // first code of the error's chain, which follows it.
            writeln!(
                std::io::stderr(),
                "NVX-CPU-PROFILE-FALLBACK: [E_PROFILE_UNSUPPORTED] the test's warning\n\
                 fatal error: [{code}] failed to launch vm worker\n\nCaused by:\n    \
                 0: [{code}] the test's restore"
            )
            .unwrap();
        }
        if let Some(port) = words.iter().find_map(|word| word.strip_prefix("forward:")) {
            // OpenVMM gives the error no code, and numbers the causes of its chain.
            writeln!(
                std::io::stderr(),
                "warning: the test's forward (Tcp 127.0.0.1:{port}) is not yet bound\n\
                 fatal error: failed to launch vm worker\n\nCaused by:\n    \
                 0: failed to resolve the test's network endpoint\n    \
                 1: failed to create socket for port forward (Tcp 127.0.0.1:{port})\n    \
                 2: the test's port is held"
            )
            .unwrap();
        }
        write_report(&words, &arguments);
        return;
    }
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
    write_report(&words, &arguments);
    if let Some(held) = held {
        held.release();
    }
}

/// Writes OpenVMM's outcome report that the word `report:CATEGORY:STATUS` among `words` of a
/// [`restore_vmm_role`]'s plan asks for, if any: a VM whose outcome is of the category
/// `CATEGORY`, with the status `STATUS`, at the `--microvm-report` path among `arguments`, as
/// OpenVMM writes it before it exits.
fn write_report(words: &[&str], arguments: &[String]) {
    let Some(report) = words.iter().find_map(|word| word.strip_prefix("report:")) else {
        return;
    };
    let (category, status) = report.split_once(':').unwrap();
    let status: i64 = status.parse().unwrap();
    let path = argument(arguments, "--microvm-report").unwrap();
    let report = serde_json::json!({
        "schema_version": 1,
        "outcome": {"operation": "managed", "category": category, "status_code": status},
        "teardown": {"workers": true},
    });
    fs::write(path, serde_json::to_vec(&report).unwrap()).unwrap();
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
    // It restores the generation's snapshot in place, with the clone's devices: its image, and its
    // `scratch` block device, which an upper in RAM declares absent, so that OpenVMM attaches it
    // with no file. On Linux, its control console listens beside the snapshot, at the socket that
    // the lease reserved, named after the launch.
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
    let (profile, processors) = (key.cpu_profile(), key.processors().to_string());
    let listen = format!("listen={endpoint}");
    let report = sandbox.join(OUTCOME_NAME);
    let expected = [
        "--machine",
        "microvm",
        "--microvm-sandbox-block",
        &distro,
        "--restore-absent-scratch",
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
fn a_clone_of_a_template_with_a_nic_restores_its_own_policy_in_place_of_the_templates() {
    let fixture = network_fixture();
    complete_snapshot(&fixture);
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    // The class's network allows egress; the clone's denies it but to one destination, and
    // publishes the guest's listeners on host loopback, with a forward.
    let mut policy = NetworkPolicy::new(NetworkAccess::Deny)
        .with_allow(EgressRule::to("192.0.2.0/24").on_port(Protocol::Tcp, 443));
    policy.host_loopback = HostLoopback::Allow;
    policy.host_loopback_forwards.push(LoopbackForward {
        protocol: ForwardProtocol::Tcp,
        host_port: 18080,
        guest_port: 8080,
    });
    let attached = SandboxNetwork::new("10.0.0.2/24", policy).unwrap();
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
    // After the image, the devices end with `--restore-network-policy` and the options of the
    // clone's own policy, without `--net` and the guest's address, which OpenVMM takes from the
    // snapshot, and refuses beside it, then with `--restore-absent-scratch`, which declares the
    // scratch of the clone's upper in RAM absent.
    let mut expected = attached.openvmm_arguments().unwrap();
    assert_eq!(expected[..2], ["--net", "10.0.0.2/24"]);
    expected.splice(..2, ["--restore-network-policy".to_owned()]);
    expected.push("--restore-absent-scratch".to_owned());
    let arguments = made.take().unwrap().arguments;
    let devices = arguments
        .iter()
        .position(|argument| argument == "--single-process")
        .unwrap();
    assert_eq!(arguments[4..devices], expected);
    assert!(!arguments.iter().any(|argument| argument == "--net"));
}

#[test]
fn a_clone_of_a_template_with_processors_offline_at_boot_restores_its_processor_target() {
    let fixture = Fixture::with(|class, spec, _| set_processors(class, spec, 4, 1));
    let (snapshot, _) = complete_snapshot(&fixture);
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let request = CloneRequest {
        processors: 2,
        ..tmpfs_request(&image, &binding)
    };
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
    // The VM keeps the template's processor capacity, of which the restore brings the target's
    // prefix online. The clone in RAM declares the template's scratch absent among its devices,
    // as any clone in RAM does.
    let arguments = made.take().unwrap().arguments;
    let at = arguments
        .iter()
        .position(|argument| argument == "--processors")
        .unwrap();
    assert!(
        arguments[..at]
            .iter()
            .any(|argument| argument == "--restore-absent-scratch"),
        "{arguments:?}"
    );
    assert_eq!(
        arguments[at..at + 8],
        [
            "--processors",
            "4",
            "--restore-snapshot",
            snapshot.to_str().unwrap(),
            "--restore-processors",
            "2",
            "--restore-memory",
            "512M",
        ]
    );
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
    // descriptor or handle that holds the claim, which no argument names. The scratch is not
    // absent.
    let made = made.take().unwrap();
    assert_eq!(made.inherited, 1);
    let device = format!("scratch:file:{}", shown(&scratch));
    assert!(made.arguments.contains(&device), "{:?}", made.arguments);
    assert!(
        !made
            .arguments
            .iter()
            .any(|argument| argument == "--restore-absent-scratch"),
        "{:?}",
        made.arguments
    );
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
fn a_clone_with_an_export_binds_it_to_its_template_s_dormant_filesystem_slot() {
    let fixture = Fixture::new();
    complete_snapshot(&fixture);
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let (export, _) = fixture.aggregate_export("aggregate");
    let request = with_export(tmpfs_request(&image, &binding), &export);
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
    // The clone in RAM declares its scratch absent, and the export follows the clone's block
    // device, as a cold launch attaches it: the aggregate, on the dormant slot, with its
    // directories and files, their modes and policies.
    let arguments = made.take().unwrap().arguments;
    let devices = arguments
        .iter()
        .position(|argument| argument == "--single-process")
        .unwrap();
    assert_eq!(arguments[4], "--restore-absent-scratch");
    assert_eq!(arguments[5..devices], export.openvmm_arguments());
    assert_eq!(
        arguments[5..7],
        ["--mount-aggregate", filesystem::GUEST_EXPORT]
    );
    // The restore sequence sends the export's table once the clone is ready, as a cold launch's
    // guest receives it.
    assert_eq!(restoring.table, export.guest_table());
    assert_eq!(
        restoring.abandon().unwrap(),
        Recovery::Terminated(reference)
    );
}

#[test]
fn a_clone_whose_exported_objects_changed_since_its_plan_starts_nothing() {
    let fixture = Fixture::new();
    complete_snapshot(&fixture);
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let request = tmpfs_request(&image, &binding);
    let runtime = runtime_of(&fixture);
    let hooks = Hooks {
        command: &never_spawned,
        ..Hooks::LAUNCH
    };
    let changed = |path: &Path| {
        (
            ErrorKind::Other,
            format!(
                "{} no longer names the host object it named at provision; deprovision the \
                 sandbox and provision it again",
                path.display()
            ),
        )
    };
    // A directory that the guest writes, in which it maps a directory that a workload could
    // move, which another directory has replaced since the plan.
    let mut moved = fixture.acquire("moved");
    let data = fixture.exported("moved", &["inner"]);
    let inner = data.join("inner");
    let export = export_plan(
        &[(&data, "/data", false), (&inner, "/inner", false)],
        &[],
        &[],
    );
    assert!(export.binds[1].identity.is_some());
    let plan = fixture
        .plan(
            &mut moved,
            &with_export(request.clone(), &export),
            NO_CONTENT,
        )
        .unwrap();
    fs::rename(&inner, data.join("renamed")).unwrap();
    fs::create_dir(&inner).unwrap();
    assert_eq!(
        refused_at(
            plan.launch_with(&mut moved, Path::new(OPENVMM), &runtime, hooks),
            Stage::Launch,
            Evidence::InvalidMappings,
        ),
        changed(&inner)
    );
    assert_unlaunched(&fixture, &moved);
    // The exported directory, which is missing since the plan.
    let mut missing = fixture.acquire("missing");
    let export = fixture.data_export("missing");
    let data = export.children[0].root.clone();
    let plan = fixture
        .plan(
            &mut missing,
            &with_export(request.clone(), &export),
            NO_CONTENT,
        )
        .unwrap();
    fs::remove_dir_all(&data).unwrap();
    assert_eq!(
        refused_at(
            plan.launch_with(&mut missing, Path::new(OPENVMM), &runtime, hooks),
            Stage::Launch,
            Evidence::InvalidMappings,
        ),
        changed(&data)
    );
    assert_unlaunched(&fixture, &missing);
    // The exported directory, which a file has replaced since the plan.
    let mut replaced = fixture.acquire("replaced");
    let export = fixture.data_export("replaced");
    let data = export.children[0].root.clone();
    let plan = fixture
        .plan(
            &mut replaced,
            &with_export(request.clone(), &export),
            NO_CONTENT,
        )
        .unwrap();
    fs::remove_dir_all(&data).unwrap();
    fs::write(&data, b"file").unwrap();
    assert_eq!(
        refused_at(
            plan.launch_with(&mut replaced, Path::new(OPENVMM), &runtime, hooks),
            Stage::Launch,
            Evidence::InvalidMappings,
        ),
        changed(&data)
    );
    assert_unlaunched(&fixture, &replaced);
    // A file of an aggregate, which a directory has replaced since the plan, and which would
    // grant the whole directory.
    let mut swapped = fixture.acquire("swapped");
    let (export, [_, notes, ..]) = fixture.aggregate_export("swapped");
    let plan = fixture
        .plan(&mut swapped, &with_export(request, &export), NO_CONTENT)
        .unwrap();
    fs::remove_file(&notes).unwrap();
    fs::create_dir(&notes).unwrap();
    assert_eq!(
        refused_at(
            plan.launch_with(&mut swapped, Path::new(OPENVMM), &runtime, hooks),
            Stage::Launch,
            Evidence::InvalidMappings,
        ),
        changed(&notes)
    );
    assert_unlaunched(&fixture, &swapped);
}

#[test]
fn a_clone_whose_export_cannot_be_inspected_starts_nothing_and_fails_with_the_host_s_evidence() {
    let fixture = Fixture::new();
    complete_snapshot(&fixture);
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let request = tmpfs_request(&image, &binding);
    let runtime = runtime_of(&fixture);
    let hooks = Hooks {
        command: &never_spawned,
        ..Hooks::LAUNCH
    };
    // The plan pins an object at a name that the host's file system cannot hold: the failure to
    // inspect it shows nothing of the object, so it is the host's own, not the request's.
    let mut acquired = fixture.acquire("uninspected");
    let data = fixture.exported("uninspected", &["inner"]);
    let inner = data.join("inner");
    let mut export = export_plan(
        &[(&data, "/data", false), (&inner, "/inner", false)],
        &[],
        &[],
    );
    assert!(export.binds[1].identity.is_some());
    export.binds[1].source = "n".repeat(300);
    let unnamed = export.children[0].root.join(&export.binds[1].source);
    let plan = fixture
        .plan(&mut acquired, &with_export(request, &export), NO_CONTENT)
        .unwrap();
    assert_eq!(
        refused_at(
            plan.launch_with(&mut acquired, Path::new(OPENVMM), &runtime, hooks),
            Stage::Launch,
            Evidence::HostFailure,
        ),
        (
            ErrorKind::Io,
            format!("cannot inspect {}", unnamed.display())
        )
    );
    assert_unlaunched(&fixture, &acquired);
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
        refused_at(
            plan.launch_with(&mut unbound, openvmm, &runtime, hooks),
            Stage::Launch,
            Evidence::HostFailure
        ),
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
        refused_at(
            plan.launch_with(&mut second, openvmm, &runtime, hooks),
            Stage::Launch,
            Evidence::HostFailure
        ),
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
    let launched = plan.launch_with(&mut first, openvmm, &other, hooks);
    let (kind, message) = refused_at(launched, Stage::Launch, Evidence::RuntimeMismatch);
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
            refused_at(
                plan.launch_with(&mut deep, openvmm, &runtime, hooks),
                Stage::Launch,
                Evidence::EndpointBind
            ),
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

/// Returns OpenVMM's outcome report of a VM whose guest ended it with `status`.
fn guest_exit(status: u8) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "outcome": {"operation": "managed", "category": "guest-exit", "status_code": status},
        "teardown": {"workers": true},
    }))
    .unwrap()
}

#[test]
fn a_clone_whose_launch_failed_before_it_began_launches_no_more_until_its_release() {
    let fixture = Fixture::new();
    complete_snapshot(&fixture);
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let request = tmpfs_request(&image, &binding);
    let runtime = runtime_of(&fixture);
    let openvmm = Path::new(OPENVMM);
    // The launch fails once it wrote the clone's files and recorded its progress, before it
    // began, which leaves the lock's reservation.
    let interrupt = |point: Point| match point {
        Point::Prepared => Err(Error::io("the test fails the launch")),
        Point::Begun => Ok(()),
    };
    let hooks = Hooks {
        command: &never_spawned,
        interrupt: &interrupt,
        ..Hooks::LAUNCH
    };
    let mut acquired = fixture.acquire("failed");
    // The report that an earlier VM of the sandbox left, which the acquisition kept, is not the
    // clone's: an inspection takes none, and the launch removes it before it records anything.
    let report = acquired.locked.outcome_path();
    fs::write(&report, guest_exit(195)).unwrap();
    assert!(crate::host::report::read(&report).is_some());
    let records = CloneRecords::read(&acquired.locked).unwrap().unwrap();
    assert_eq!(
        (records.progress, records.report),
        (Progress::Planned, None)
    );
    let plan = fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap();
    let failure = plan
        .launch_with(&mut acquired, openvmm, &runtime, hooks)
        .unwrap_err();
    assert_recorded(
        &failure,
        &acquired.lease,
        &acquired.locked,
        (Stage::Launch, Progress::Launching),
        Evidence::HostFailure,
        Teardown::NoVm,
    );
    assert!(!exists(&report));
    assert_eq!(acquired.locked.reserved(), Some(acquired.lease.launch()));
    // Another launch under the lease and the lock that still reserves it is refused before it
    // writes anything, so the record of the failure stays as it was.
    let path = acquired.locked.clone_failure_path();
    let recorded = fs::read(&path).unwrap();
    let plan = fixture.plan(&mut acquired, &request, NO_CONTENT).unwrap();
    assert_eq!(
        refused_at(
            plan.launch_with(&mut acquired, openvmm, &runtime, hooks),
            Stage::Launch,
            Evidence::HostFailure
        ),
        (
            ErrorKind::Other,
            format!(
                "the clone of the lease {} failed, as {} records: a lease's clone fails once, \
                 and the release of the lease ends it",
                acquired.lease.id(),
                path.display()
            )
        )
    );
    assert_eq!(fs::read(&path).unwrap(), recorded);
    // The release ends the lease with the record.
    assert_eq!(
        fixture.store.release(&mut acquired.locked).unwrap(),
        Some(acquired.lease.clone())
    );
    assert!(!path.exists());
}

#[test]
fn a_failure_that_a_reattach_records_before_any_launch_shows_no_report_of_an_earlier_vm() {
    // A reattach before any launch of the clone that cannot read the record of a failure, as a
    // transient error of the host may keep it from reading one, records a failure of its own, at
    // the plan, with no VM: the report that an earlier VM of the sandbox left, which no launch
    // removed, stays none of the clone's.
    let fixture = Fixture::new();
    let runtime = runtime_of(&fixture);
    let mut acquired = fixture.acquire("sandbox");
    let locked = &mut acquired.locked;
    fs::write(locked.outcome_path(), guest_exit(195)).unwrap();
    write_private(&locked.clone_failure_path(), b"not a record").unwrap();
    let mut control = ReattachControl::answering(Some);
    let deadline = Instant::now() + TIMEOUT;
    let reattached = reattach_clone(locked, &runtime, &mut control, deadline, TIMEOUT).unwrap();
    let failure = failed(reattached);
    assert_eq!(
        (failure.record().progress, &failure.record().teardown),
        (Progress::Planned, &Teardown::NoVm)
    );
    let records = CloneRecords::read(locked).unwrap().unwrap();
    assert_eq!(records.failure.as_ref(), Some(failure.record()));
    assert_eq!(
        (records.progress, records.report),
        (Progress::Planned, None)
    );
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
    let launch_refusal = |fixture: &Fixture, sandbox: &str, evidence: Evidence| {
        complete_snapshot(fixture);
        let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
        let mut acquired = fixture.acquire(sandbox);
        let plan = fixture
            .plan(&mut acquired, &tmpfs_request(&image, &binding), NO_CONTENT)
            .unwrap();
        let runtime = runtime_of(fixture);
        let launched = plan.launch_with(&mut acquired, Path::new(OPENVMM), &runtime, hooks);
        let refusal = refused_at(launched, Stage::Launch, evidence);
        assert_unlaunched(fixture, &acquired);
        (refusal, fixture.store.clone_socket_path(&acquired.lease))
    };
    // A store whose path leaves no room for the socket of a clone's VMM in a payload.
    let fixture = Fixture::in_store(|directory| deep(directory).join("templates"), |_, _, _| {});
    let ((kind, message), socket) = launch_refusal(&fixture, "sandbox", Evidence::EndpointBind);
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
    let ((kind, message), socket) = launch_refusal(&fixture, "sandbox", Evidence::EndpointBind);
    assert!(socket.as_os_str().len() < 108, "{}", socket.display());
    let resolved = filesystem::canonicalize(socket.parent().unwrap())
        .unwrap()
        .join(socket.file_name().unwrap());
    assert_eq!(
        (kind, message),
        (ErrorKind::Unsupported, too_long(&resolved))
    );
    // A store whose path holds a comma or an equals sign, which OpenVMM's option `listen=<path>`
    // takes for the end of the path.
    for name in ["a,b", "a=b"] {
        let fixture = Fixture::in_store(
            |directory| {
                let parent = directory.join(name);
                assert_eq!(platform::create_owner_only_dir(&parent).unwrap(), None);
                parent.join("templates")
            },
            |_, _, _| {},
        );
        let bind = Evidence::EndpointBind;
        let ((kind, message), socket) = launch_refusal(&fixture, "sandbox", bind);
        assert_eq!(
            (kind, message),
            (
                ErrorKind::Unsupported,
                format!(
                    "the path of the control socket {} of a clone's VMM must hold no ',' or '=', \
                     which end the path in OpenVMM's option `listen=<path>`",
                    socket.display()
                )
            )
        );
    }
    // A store whose path is not valid UTF-8, as OpenVMM takes the paths of its sockets.
    let fixture = Fixture::in_store(
        |directory| {
            use std::os::unix::ffi::OsStrExt;

            let parent = directory.join(std::ffi::OsStr::from_bytes(b"a\xffb"));
            assert_eq!(platform::create_owner_only_dir(&parent).unwrap(), None);
            parent.join("templates")
        },
        |_, _, _| {},
    );
    let ((kind, message), socket) = launch_refusal(&fixture, "sandbox", Evidence::EndpointBind);
    assert!(socket.to_str().is_none(), "{}", socket.display());
    assert_eq!(
        (kind, message),
        (
            ErrorKind::Unsupported,
            format!(
                "the path of the socket {} of a clone's launch must be valid UTF-8, as OpenVMM \
                 takes it",
                socket.display()
            )
        )
    );
    // A store whose path is valid UTF-8, but not once its links are resolved, as OpenVMM takes
    // the path at which it listens.
    let fixture = Fixture::in_store(
        |directory| {
            use std::os::unix::ffi::OsStrExt;

            let target = directory.join(std::ffi::OsStr::from_bytes(b"a\xffb"));
            assert_eq!(platform::create_owner_only_dir(&target).unwrap(), None);
            let link = directory.join("u");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            link.join("templates")
        },
        |_, _, _| {},
    );
    let ((kind, message), socket) = launch_refusal(&fixture, "sandbox", Evidence::EndpointBind);
    assert!(socket.to_str().is_some(), "{}", socket.display());
    let resolved = filesystem::canonicalize(socket.parent().unwrap())
        .unwrap()
        .join(socket.file_name().unwrap());
    assert!(resolved.to_str().is_none(), "{}", resolved.display());
    assert_eq!(
        (kind, message),
        (
            ErrorKind::Unsupported,
            format!(
                "the path of the socket {} of a clone's launch must be valid UTF-8, as OpenVMM \
                 takes it",
                resolved.display()
            )
        )
    );
    // A payload private to the current user, as a plan requires, but whose mode is not 0700, as
    // OpenVMM requires of the directory of its control socket. An acquisition refuses such a
    // payload too, so its mode changes here once the lease is granted.
    let fixture = Fixture::new();
    complete_snapshot(&fixture);
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("sandbox");
    let plan = fixture
        .plan(&mut acquired, &tmpfs_request(&image, &binding), NO_CONTENT)
        .unwrap();
    let payload = fixture.store.payload_dir(fixture.generation.generation());
    fs::set_permissions(&payload, fs::Permissions::from_mode(0o500)).unwrap();
    let launched = plan.launch_with(
        &mut acquired,
        Path::new(OPENVMM),
        &runtime_of(&fixture),
        hooks,
    );
    let refusal = refused_at(launched, Stage::Launch, Evidence::MalformedGeneration);
    fs::set_permissions(&payload, fs::Permissions::from_mode(0o700)).unwrap();
    assert_unlaunched(&fixture, &acquired);
    assert_eq!(
        refusal,
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
    let journal = sandbox.join(JOURNAL_NAME);
    let kept = sandbox.join(format!("{JOURNAL_NAME}.kept"));
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
        ..Hooks::LAUNCH
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
    if step == "superseded" {
        fs::hard_link(&journal, &kept).unwrap();
    }
    let committed = started.commit().unwrap();
    if step == "superseded" {
        // The commit's removal of the journal failed, as a commit tolerates: the journal, the
        // same file, remains beside the runtime record that supersedes it.
        fs::rename(&kept, &journal).unwrap();
        crash(&step);
    }
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

    fn map(
        &mut self,
        _session: &mut Box<dyn Transport>,
        _table: &[HostMap],
        _deadline: Instant,
    ) -> io::Result<()> {
        // The role's clone has no export, so its sequence sends no mapping table.
        unreachable!("a clone without an export got a mapping table")
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
    let mut command = Command::new(env::current_exe().unwrap());
    command
        .args(role("launch_role"))
        .env(ROLE_STORE, fixture.store.dir())
        .env(ROLE_GENERATION, fixture.generation.generation().to_string())
        .env(ROLE_SANDBOX, sandbox)
        .env(ROLE_IMAGE, image)
        .env(ROLE_CRASH, step)
        .env(ROLE_VMM, vmm)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = supervisor::spawn_command(&mut command)
        .unwrap()
        .wait_with_output()
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
        Self::at_with(step, "")
    }

    /// Leases a fresh fixture's generation to a new sandbox, whose clone a [`launch_role`]
    /// launches until it crashes at `step`, with a VMM whose [plan](VmmDir::plan) holds `words`.
    fn at_with(step: &str, words: &str) -> Self {
        let fixture = Fixture::new();
        complete_snapshot(&fixture);
        let (image, _) = fixture.image("distro.erofs", IMAGE_LEN);
        let sandbox = fixture.provision("sandbox");
        let mut vmm = VmmDir::new();
        let mut plan = words.to_owned();
        if !matches!(step, "prepared" | "begun" | "launched") {
            // The restore sequence reads the event, whose flush waits for that on Windows.
            plan.push_str(" flush");
        }
        vmm.plan(&plan);
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

    /// Takes the clone of the sandbox of `locked` over, under `runtime`, through `control`, with
    /// [`reattach_clone`], until the result is determinate, as a caller retries an indeterminate
    /// reattach, and returns it.
    fn take_over<C: CloneControl>(
        &self,
        locked: &mut LockedStore,
        runtime: &RuntimeIdentity,
        control: &mut C,
    ) -> ReattachedClone<C::Session> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let session = Instant::now() + TIMEOUT;
            match reattach_clone(locked, runtime, control, session, TIMEOUT).unwrap() {
                ReattachedClone::Indeterminate(reason) if Instant::now() < deadline => {
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
        assert!(CloneRecords::read(locked).unwrap().is_none());
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
fn a_launch_that_began_shows_the_report_of_a_vmm_that_its_journal_does_not_record() {
    // The caller crashed once the launch began, before the journal recorded the VMM that it
    // spawned, which left the report: the clone's, since the launch removed any earlier VM's
    // report before it recorded anything, though the clone's progress shows no spawn. An
    // inspection takes the report once a reattach of the clone ended the launch, recording its
    // failure, and once a recovery of the sandbox's launch did, recording none.
    let shown = |records: &CloneRecords| {
        let report = records
            .report
            .as_ref()
            .expect("the report of the clone's VM");
        (report.category.clone(), report.status_code)
    };
    let ended = (Some("guest-exit".to_owned()), Some(3));
    for of_the_clone in [true, false] {
        let crashed = Crashed::at("begun");
        let mut locked = crashed.lock();
        fs::write(locked.outcome_path(), guest_exit(3)).unwrap();
        let records = CloneRecords::read(&locked).unwrap().unwrap();
        assert_eq!(
            (records.progress, records.journal.is_some()),
            (Progress::Launching, true)
        );
        assert_eq!(shown(&records), ended);
        if of_the_clone {
            let mut control = ReattachControl::answering(Some);
            let failure = failed(crashed.take_over(&mut locked, &crashed.runtime, &mut control));
            assert_eq!(failure.record().teardown, Teardown::Exited);
        } else {
            match crashed.reattach(&mut locked) {
                Reattached::Exited {
                    record: None,
                    recovery: Recovery::Exited,
                } => {}
                other => panic!("unexpected reattach: {other:?}"),
            }
        }
        let records = CloneRecords::read(&locked).unwrap().unwrap();
        assert_eq!(
            (
                records.progress,
                records.journal.is_none(),
                records.failure.is_some()
            ),
            (Progress::Launching, true, of_the_clone)
        );
        assert_eq!(shown(&records), ended, "{of_the_clone}");
        crashed.releases(&mut locked);
    }
}

#[test]
fn a_reattach_takes_the_report_of_a_vmm_whose_launch_a_recovery_of_the_sandbox_ended() {
    // The caller crashed once the launch began, before the journal recorded the VMM that it
    // spawned, which left the report, and a recovery of the sandbox's launch then removed the
    // journal: nothing records the spawn, but the report is the clone's VM's, as an inspection
    // takes it, so a reattach of the clone takes it too.
    let recovered = |status: u8| {
        let crashed = Crashed::at("begun");
        let mut locked = crashed.lock();
        fs::write(locked.outcome_path(), guest_exit(status)).unwrap();
        match crashed.reattach(&mut locked) {
            Reattached::Exited {
                record: None,
                recovery: Recovery::Exited,
            } => {}
            other => panic!("unexpected reattach: {other:?}"),
        }
        assert!(locked.journal().unwrap().is_none());
        (crashed, locked)
    };
    let mut control = ReattachControl::answering(Some);
    // The status of the guest's repair is evidence of the clone's failure at any point.
    let (crashed, mut locked) = recovered(195);
    let failure = failed(crashed.take_over(&mut locked, &crashed.runtime, &mut control));
    assert_eq!(failure.kind(), ErrorKind::Exited, "{failure}");
    assert_recorded(
        &failure,
        &crashed.lease,
        &locked,
        (Stage::Launch, Progress::Launching),
        Evidence::RepairStatus,
        Teardown::Exited,
    );
    assert_eq!(failure.record().status, Some(195));
    crashed.releases(&mut locked);
    // Another status is no evidence of a failure: the clone ended, with the report of its VM.
    let (crashed, mut locked) = recovered(3);
    match crashed.take_over(&mut locked, &crashed.runtime, &mut control) {
        ReattachedClone::Ended {
            progress: Progress::Launching,
            report: Some(report),
        } => {
            let outcome = (report.category.as_deref(), report.status_code);
            assert_eq!(outcome, (Some("guest-exit"), Some(3)));
        }
        other => panic!("the reattach found {}", describe(&other)),
    }
    let records = CloneRecords::read(&locked).unwrap().unwrap();
    assert!(records.failure.is_none());
    crashed.releases(&mut locked);
    assert!(control.capabilities.is_empty());
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

/// The Ready of a clone that a [`ReattachControl`] answers, from the activation of the record that
/// the clone's Activate carries.
type Answered = fn(ReadyActivation) -> Option<ReadyActivation>;

/// The adapter's framing of the sessions of a reattach: it attaches a session to each connection
/// to the clone's control endpoint, keeps the capability of each, and the record of each Activate,
/// which it answers as its `answer` says.
struct ReattachControl {
    /// The capabilities of the sessions, in order.
    capabilities: Vec<[u8; CAPABILITY_LEN]>,
    /// The records of the Activates, in order.
    records: Vec<Vec<u8>>,
    /// How the clone answers an Activate.
    answer: Answered,
}

impl ReattachControl {
    /// Returns a framing whose clone answers each Activate as `answer` says.
    fn answering(answer: Answered) -> Self {
        Self {
            capabilities: Vec::new(),
            records: Vec::new(),
            answer,
        }
    }
}

impl CloneControl for ReattachControl {
    type Session = Box<dyn Transport>;
    type Error = io::Error;

    fn attach(
        &mut self,
        transport: Box<dyn Transport>,
        capability: &[u8; CAPABILITY_LEN],
        _deadline: Instant,
    ) -> io::Result<Box<dyn Transport>> {
        self.capabilities.push(*capability);
        Ok(transport)
    }

    fn activate(
        &mut self,
        _session: &mut Box<dyn Transport>,
        record: &[u8],
        _deadline: Instant,
    ) -> io::Result<Option<ReadyActivation>> {
        self.records.push(record.to_vec());
        let record = ActivationRecord::decode(record).unwrap();
        Ok((self.answer)(ReadyActivation::of(&record)))
    }

    fn map(
        &mut self,
        _session: &mut Box<dyn Transport>,
        _table: &[HostMap],
        _deadline: Instant,
    ) -> io::Result<()> {
        panic!("a reattach sends no mapping table")
    }

    fn start(&mut self, _session: &mut Box<dyn Transport>, _deadline: Instant) -> io::Result<()> {
        panic!("a reattach starts no sandbox")
    }

    fn lost(&self, _error: &io::Error) -> bool {
        false
    }
}

#[test]
fn a_clone_whose_caller_crashed_once_it_committed_is_taken_over_in_a_new_session() {
    let crashed = Crashed::at("committed");
    let reference = crashed.vmm.reference.clone().unwrap();
    let started = crashed.vmm.started();
    let (capability, _) = started.split_once(' ').unwrap();
    // A tombstone quarantines the clone's generation, which a retirement deletes once its leases
    // have drained: neither takes the clone's lease from it.
    let store = &crashed.fixture.store;
    let generation = crashed.fixture.generation.generation();
    store.tombstone(generation, None).unwrap();
    assert_eq!(
        store.retire(generation, None).unwrap(),
        Retirement::Draining(vec![crashed.lease.clone()])
    );
    let mut locked = crashed.lock();
    let recorded = fs::read(locked.clone_activation_path()).unwrap();
    let mut control = ReattachControl::answering(Some);
    let (vmm, session) = match crashed.take_over(&mut locked, &crashed.runtime, &mut control) {
        ReattachedClone::Live { vmm, session } => (vmm, session),
        other => panic!("the reattach found {}", describe(&other)),
    };
    // The VM is the one that the launch committed, whose clone answered the Activate of the
    // sandbox's record, byte for byte, in a session that the launch's capability authenticated,
    // and runs on, with its lease.
    assert_eq!(vmm.process().reference(), &reference);
    assert_eq!(control.records, [recorded]);
    let attached: Vec<String> = control.capabilities.iter().map(|key| hex(key)).collect();
    assert_eq!(attached, [capability]);
    drop(session);
    let records = CloneRecords::read(&locked).unwrap().unwrap();
    assert_eq!(
        (records.progress, records.failure),
        (Progress::Committed, None)
    );
    crashed.refuses_release(&mut locked);
    assert_eq!(
        vmm.terminate(&mut locked, TIMEOUT).unwrap(),
        StopOutcome::Forced
    );
    crashed.releases(&mut locked);
    // Its lease released, the retirement deletes the generation.
    assert_eq!(store.retire(generation, None).unwrap(), Retirement::Retired);
}

#[test]
fn a_journal_that_a_clone_s_commit_left_is_no_failure_of_the_clone() {
    // The commit of the clone's launch could not remove the launch's journal, which remains
    // beside the runtime record of the same launch, as its caller crashed.
    let crashed = Crashed::at("superseded");
    let mut locked = crashed.lock();
    let journal = locked
        .journal()
        .unwrap()
        .expect("the commit left the journal");
    let runtime = locked
        .runtime()
        .unwrap()
        .expect("the commit wrote its record");
    assert_eq!(journal.launch(), runtime.launch());
    // Each reattach takes the clone over, and records no failure: the first removes the journal.
    let mut control = ReattachControl::answering(Some);
    let mut live = None;
    for attempt in 0..2 {
        let vmm = match crashed.take_over(&mut locked, &crashed.runtime, &mut control) {
            ReattachedClone::Live { vmm, .. } => vmm,
            other => panic!("the reattach {attempt} found {}", describe(&other)),
        };
        assert_eq!(
            Some(vmm.process().reference()),
            crashed.vmm.reference.as_ref()
        );
        let records = CloneRecords::read(&locked).unwrap().unwrap();
        assert_eq!(
            (records.progress, records.failure, records.journal.is_some()),
            (Progress::Committed, None, false),
            "{attempt}"
        );
        live = Some(vmm);
    }
    assert_eq!(control.records.len(), 2);
    crashed.refuses_release(&mut locked);
    let vmm = live.unwrap();
    assert_eq!(
        vmm.terminate(&mut locked, TIMEOUT).unwrap(),
        StopOutcome::Forced
    );
    crashed.releases(&mut locked);
}

#[test]
fn a_reattach_ends_a_clone_that_it_may_not_keep_through_its_verified_identity() {
    let reattach = (Stage::Reattach, Progress::Committed);
    // A clone that runs under another runtime identity, which might not speak this one's
    // protocol: the reattach sends its guest nothing.
    let crashed = Crashed::at("committed");
    let mut locked = crashed.lock();
    let other = RuntimeIdentity::new(artifact('d'), artifact('e'), artifact('f'));
    let mut control = ReattachControl::answering(Some);
    let failure = failed(crashed.take_over(&mut locked, &other, &mut control));
    assert_eq!(failure.code(), ErrorCode::RuntimeMismatch, "{failure}");
    let (lease, terminated) = (&crashed.lease, Teardown::Terminated);
    let mismatch = Evidence::RuntimeMismatch;
    assert_recorded(&failure, lease, &locked, reattach, mismatch, terminated);
    assert_eq!(failure.outcome(), Outcome::Host);
    assert!(control.capabilities.is_empty());
    assert!(locked.runtime().unwrap().is_none());
    crashed.releases(&mut locked);
    // A clone whose Ready reports another activation than its record's.
    let crashed = Crashed::at("committed");
    let mut locked = crashed.lock();
    let mut control = ReattachControl::answering(|_| Some(ReadyActivation { transaction: None }));
    let failure = failed(crashed.take_over(&mut locked, &crashed.runtime, &mut control));
    assert_eq!(failure.kind(), ErrorKind::Protocol, "{failure}");
    let (lease, terminated) = (&crashed.lease, Teardown::Terminated);
    assert_recorded(
        &failure,
        lease,
        &locked,
        reattach,
        Evidence::Protocol,
        terminated,
    );
    assert_eq!(control.records.len(), 1);
    assert!(locked.runtime().unwrap().is_none());
    crashed.releases(&mut locked);
}

#[test]
fn a_reattach_fails_a_clone_whose_record_of_a_failure_cannot_be_read() {
    let reattach = (Stage::Reattach, Progress::Committed);
    let host = Evidence::HostFailure;
    // A record that is malformed, and one of a format that this build does not read: the clone
    // failed, so the reattach sends its guest nothing, and ends its VM through its verified
    // identity, though its Ready would match its activation record.
    let unreadable: [(&[u8], ErrorCode); 2] = [
        (b"not a record", ErrorCode::Io),
        (br#"{"format": 99}"#, ErrorCode::FormatUnsupported),
    ];
    for (bytes, code) in unreadable {
        let crashed = Crashed::at("committed");
        let mut locked = crashed.lock();
        write_private(&locked.clone_failure_path(), bytes).unwrap();
        let mut control = ReattachControl::answering(Some);
        let failure = failed(crashed.take_over(&mut locked, &crashed.runtime, &mut control));
        assert_eq!(failure.code(), code, "{failure}");
        let (lease, terminated) = (&crashed.lease, Teardown::Terminated);
        assert_recorded(&failure, lease, &locked, reattach, host, terminated);
        assert!(control.capabilities.is_empty());
        assert!(locked.runtime().unwrap().is_none());
        crashed.releases(&mut locked);
    }
    // A clone whose VM exited with a status that is no evidence of a failure failed all the same.
    let crashed = Crashed::at_with("committed", "report:guest-exit:3");
    crashed.vmm.end_and_wait();
    let mut locked = crashed.lock();
    write_private(&locked.clone_failure_path(), b"not a record").unwrap();
    let mut control = ReattachControl::answering(Some);
    let failure = failed(crashed.take_over(&mut locked, &crashed.runtime, &mut control));
    let (lease, exited) = (&crashed.lease, Teardown::Exited);
    assert_recorded(&failure, lease, &locked, reattach, host, exited);
    assert_eq!(failure.record().status, Some(3));
    assert!(control.capabilities.is_empty());
    crashed.releases(&mut locked);
    // A clone whose VM cannot be examined failed all the same, with its teardown unresolved, which
    // a later reattach resolves.
    let crashed = Crashed::at("committed");
    let mut locked = crashed.lock();
    write_private(&locked.clone_failure_path(), b"not a record").unwrap();
    let mut control = ReattachControl::answering(Some);
    let reason = "the test cannot examine the VM";
    let reattached = reattach_clone_with(
        &mut locked,
        &crashed.runtime,
        &mut control,
        Instant::now() + TIMEOUT,
        TIMEOUT,
        |_, _, _| Ok(Reattached::Indeterminate(reason.to_owned())),
    )
    .unwrap();
    let failure = failed(reattached);
    let (lease, unresolved) = (&crashed.lease, Teardown::Unresolved(reason.to_owned()));
    assert_recorded(&failure, lease, &locked, reattach, host, unresolved);
    let failure = failed(crashed.take_over(&mut locked, &crashed.runtime, &mut control));
    let terminated = Teardown::Terminated;
    assert_recorded(&failure, lease, &locked, reattach, host, terminated);
    assert!(control.capabilities.is_empty());
    crashed.releases(&mut locked);
}

#[test]
fn a_reattach_fails_a_clone_whose_sandbox_records_the_failure_of_another_clone() {
    // A record of the failure of another lease's clone, of another launch, which leased another
    // generation of the same store, whose evidence is the template's: it tells nothing of this
    // clone, so the reattach takes it as a record that it cannot read, and the failure that it
    // records in its place, and returns, is this clone's, which quarantines nothing.
    let crashed = Crashed::at("committed");
    let mut locked = crashed.lock();
    let lease = &crashed.lease;
    let generation = lease.generation().get().checked_add(1).unwrap();
    let other = Lease::new(
        crate::host::template::LeaseId::from_bytes([0xa5; 16]),
        lease.store(),
        StoreGeneration::new(generation),
        lease.template().key,
        lease.sandbox().to_owned(),
        crate::host::store::LaunchId::generate().unwrap(),
    );
    let defect = Error::io("the test's generation is malformed");
    let at = ((Stage::Restore, Progress::Spawned), None);
    let template = (Evidence::MalformedGeneration, Teardown::Exited);
    record_earlier(&other, &locked, at, template, &defect);
    let mut control = ReattachControl::answering(Some);
    let failure = failed(crashed.take_over(&mut locked, &crashed.runtime, &mut control));
    assert_eq!(failure.code(), ErrorCode::Io, "{failure}");
    assert!(
        failure.message().contains("belongs to another clone"),
        "{failure}"
    );
    let reattach = (Stage::Reattach, Progress::Committed);
    let (host, terminated) = (Evidence::HostFailure, Teardown::Terminated);
    assert_recorded(&failure, lease, &locked, reattach, host, terminated);
    assert!(control.capabilities.is_empty());
    assert!(locked.runtime().unwrap().is_none());
    crashed.releases(&mut locked);
    drop(locked);
    let store = &crashed.fixture.store;
    assert!(!failure.quarantine(store, None).unwrap());
    assert!(store.tombstones().unwrap().is_empty());
}

#[test]
fn a_record_of_a_failure_that_cannot_be_read_is_a_failure_whatever_the_reattach_finds() {
    let reattach = (Stage::Reattach, Progress::Committed);
    // A VM that exited once its launch committed, with a status of the guest's that is evidence,
    // or without a report of the guest's own end: the clone failed before, so the failure is the
    // reattach's, with the error of the record's read, over whose evidence the status's prevails.
    let cases = [
        ("report:guest-exit:195", Evidence::RepairStatus, Some(195)),
        ("", Evidence::HostFailure, None),
    ];
    for (words, evidence, status) in cases {
        let crashed = Crashed::at_with("committed", words);
        crashed.vmm.end_and_wait();
        let mut locked = crashed.lock();
        write_private(&locked.clone_failure_path(), b"not a record").unwrap();
        let mut control = ReattachControl::answering(Some);
        let failure = failed(crashed.take_over(&mut locked, &crashed.runtime, &mut control));
        assert_eq!(failure.code(), ErrorCode::Io, "{words}: {failure}");
        let (lease, exited) = (&crashed.lease, Teardown::Exited);
        assert_recorded(&failure, lease, &locked, reattach, evidence, exited);
        assert_eq!(failure.record().status, status, "{words}");
        assert!(locked.runtime().unwrap().is_none(), "{words}");
        assert!(control.capabilities.is_empty(), "{words}");
        crashed.releases(&mut locked);
    }
    // A VM that runs under another runtime identity is ended, and the failure is the same.
    let crashed = Crashed::at("committed");
    let mut locked = crashed.lock();
    write_private(&locked.clone_failure_path(), b"not a record").unwrap();
    let other = RuntimeIdentity::new(artifact('d'), artifact('e'), artifact('f'));
    let mut control = ReattachControl::answering(Some);
    let failure = failed(crashed.take_over(&mut locked, &other, &mut control));
    assert_eq!(failure.code(), ErrorCode::Io, "{failure}");
    let (lease, terminated) = (&crashed.lease, Teardown::Terminated);
    let host = Evidence::HostFailure;
    assert_recorded(&failure, lease, &locked, reattach, host, terminated);
    assert!(control.capabilities.is_empty());
    crashed.releases(&mut locked);
    // A launch that a crash of its caller interrupted: the reattach replaces the record with its
    // own failure, pending, before its recovery ends the launch, which a crash of the restarted
    // caller leaves, for the next reattach to finish.
    let spawned = (Stage::Reattach, Progress::Spawned);
    let assert_pending = |locked: &LockedStore| {
        let pending = read_failure(&locked.clone_failure_path())
            .unwrap()
            .expect("the reattach replaced the record that it could not read");
        assert_eq!(
            (
                pending.stage,
                pending.progress,
                pending.evidence,
                pending.code
            ),
            (spawned.0, spawned.1, host, ErrorCode::Io)
        );
        assert_eq!(pending.teardown, Teardown::Pending);
    };
    let crashed = Crashed::at("launched");
    let mut locked = crashed.lock();
    write_private(&locked.clone_failure_path(), b"not a record").unwrap();
    let mut control = ReattachControl::answering(Some);
    let crash = reattach_clone_with(
        &mut locked,
        &crashed.runtime,
        &mut control,
        Instant::now() + TIMEOUT,
        TIMEOUT,
        |_, _, _| Err(Error::other("the test's caller crashes")),
    );
    assert!(crash.is_err());
    assert_pending(&locked);
    assert!(locked.journal().unwrap().is_some());
    let failure = failed(crashed.take_over(&mut locked, &crashed.runtime, &mut control));
    assert_eq!(failure.code(), ErrorCode::Io, "{failure}");
    let (lease, terminated) = (&crashed.lease, Teardown::Terminated);
    assert_recorded(&failure, lease, &locked, spawned, host, terminated);
    assert!(control.capabilities.is_empty());
    crashed.releases(&mut locked);
    // Before the restore-ready event, only the launch's journal, which the recovery removes, holds
    // the spawn: once it went, the pending record still tells how far the clone got, so the next
    // reattach takes the status with which the guest ended the VM.
    let crashed = Crashed::at_with("launched", "report:guest-exit:195");
    crashed.vmm.end_and_wait();
    let mut locked = crashed.lock();
    write_private(&locked.clone_failure_path(), b"not a record").unwrap();
    let mut control = ReattachControl::answering(Some);
    let crash = reattach_clone_with(
        &mut locked,
        &crashed.runtime,
        &mut control,
        Instant::now() + TIMEOUT,
        TIMEOUT,
        |locked, runtime, exited| {
            supervisor::reattach_reporting(locked, runtime, exited)?;
            Err(Error::other("the test's caller crashes"))
        },
    );
    assert!(crash.is_err());
    assert_pending(&locked);
    assert!(locked.journal().unwrap().is_none());
    let failure = failed(crashed.take_over(&mut locked, &crashed.runtime, &mut control));
    assert_eq!(failure.code(), ErrorCode::Io, "{failure}");
    let (lease, exited) = (&crashed.lease, Teardown::Exited);
    let repair = Evidence::RepairStatus;
    assert_recorded(&failure, lease, &locked, spawned, repair, exited);
    assert_eq!(failure.record().status, Some(195));
    assert!(control.capabilities.is_empty());
    crashed.releases(&mut locked);
    // A clone whose launch did not begin: the store holds no record of the launch that the
    // reattach would remove, so the record stays as it is, and a reattach after a crash of the
    // caller finds the clone as this one does, with no VM.
    let crashed = Crashed::at("prepared");
    let mut locked = crashed.lock();
    write_private(&locked.clone_failure_path(), b"not a record").unwrap();
    let mut control = ReattachControl::answering(Some);
    let crash = reattach_clone_with(
        &mut locked,
        &crashed.runtime,
        &mut control,
        Instant::now() + TIMEOUT,
        TIMEOUT,
        |_, _, _| Err(Error::other("the test's caller crashes")),
    );
    assert!(crash.is_err());
    let kept = fs::read(locked.clone_failure_path()).unwrap();
    assert_eq!(kept, b"not a record");
    let failure = failed(crashed.take_over(&mut locked, &crashed.runtime, &mut control));
    assert_eq!(failure.code(), ErrorCode::Io, "{failure}");
    let launching = (Stage::Reattach, Progress::Launching);
    let (lease, no_vm) = (&crashed.lease, Teardown::NoVm);
    assert_recorded(&failure, lease, &locked, launching, host, no_vm);
    assert_eq!(failure.record().status, None);
    crashed.releases(&mut locked);
    // A clone whose launch a recovery of the sandbox ended, once its caller crashed before the
    // journal recorded the VMM that it spawned, which left the report: nothing records the spawn,
    // but the report is the clone's VM's, since the launch removed any earlier VM's report before
    // it recorded anything, and the status of the guest's repair prevails.
    let crashed = Crashed::at("begun");
    let mut locked = crashed.lock();
    fs::write(locked.outcome_path(), guest_exit(195)).unwrap();
    match crashed.reattach(&mut locked) {
        Reattached::Exited {
            record: None,
            recovery: Recovery::Exited,
        } => {}
        other => panic!("unexpected reattach: {other:?}"),
    }
    write_private(&locked.clone_failure_path(), b"not a record").unwrap();
    let failure = failed(crashed.take_over(&mut locked, &crashed.runtime, &mut control));
    assert_eq!(failure.code(), ErrorCode::Io, "{failure}");
    let (lease, exited) = (&crashed.lease, Teardown::Exited);
    let repair = Evidence::RepairStatus;
    assert_recorded(&failure, lease, &locked, launching, repair, exited);
    assert_eq!(failure.record().status, Some(195));
    crashed.releases(&mut locked);
}

#[test]
fn a_reattach_ends_the_launch_of_a_caller_that_crashed_and_records_its_failure() {
    let cases = [
        (
            "begun",
            (Stage::Launch, Progress::Launching),
            Evidence::LostStatus,
            Teardown::Exited,
        ),
        (
            "launched",
            (Stage::Restore, Progress::Spawned),
            Evidence::ForcedKill,
            Teardown::Terminated,
        ),
        (
            "activated",
            (Stage::Start, Progress::Ready),
            Evidence::ForcedKill,
            Teardown::Terminated,
        ),
        (
            "started",
            (Stage::Commit, Progress::Started),
            Evidence::ForcedKill,
            Teardown::Terminated,
        ),
    ];
    for (step, at, evidence, teardown) in cases {
        let crashed = Crashed::at(step);
        let mut locked = crashed.lock();
        let mut control = ReattachControl::answering(Some);
        let failure = failed(crashed.take_over(&mut locked, &crashed.runtime, &mut control));
        assert_eq!(failure.kind(), ErrorKind::Exited, "{step}: {failure}");
        assert_recorded(&failure, &crashed.lease, &locked, at, evidence, teardown);
        assert!(control.capabilities.is_empty(), "{step}");
        // Once the launch has ended, a reattach finds its failure as it was recorded.
        let again = failed(crashed.take_over(&mut locked, &crashed.runtime, &mut control));
        assert_eq!(again.record(), failure.record(), "{step}");
        assert_eq!(again.message(), failure.record().message, "{step}");
        crashed.releases(&mut locked);
    }
    // A caller that crashed before its launch began left no VM, and no failure.
    let crashed = Crashed::at("prepared");
    let mut locked = crashed.lock();
    let mut control = ReattachControl::answering(Some);
    match crashed.take_over(&mut locked, &crashed.runtime, &mut control) {
        ReattachedClone::Ended {
            progress: Progress::Launching,
            report: None,
        } => {}
        other => panic!("the reattach found {}", describe(&other)),
    }
    crashed.releases(&mut locked);
}

#[test]
fn a_reattach_records_the_failure_of_an_interrupted_launch_before_it_ends_the_launch() {
    let crashed = Crashed::at("launched");
    let mut locked = crashed.lock();
    let mut control = ReattachControl::answering(Some);
    // The restarted caller crashes in turn as its reattach would end the interrupted launch.
    let crash = reattach_clone_with(
        &mut locked,
        &crashed.runtime,
        &mut control,
        Instant::now() + TIMEOUT,
        TIMEOUT,
        |_, _, _| Err(Error::other("the test's caller crashes")),
    );
    assert!(crash.is_err());
    // The sandbox records the failure, whose teardown is pending, beside the launch's journal,
    // which keeps the lease.
    let records = CloneRecords::read(&locked).unwrap().unwrap();
    let pending = records
        .failure
        .expect("the reattach recorded the failure before the recovery");
    assert_eq!(
        (pending.stage, pending.progress, pending.evidence),
        (Stage::Restore, Progress::Spawned, Evidence::LostStatus)
    );
    assert_eq!(pending.teardown, Teardown::Pending);
    assert!(records.journal.is_some());
    crashed.refuses_release(&mut locked);
    // The next reattach ends the launch, and resolves the teardown of the recorded failure.
    let failure = failed(crashed.take_over(&mut locked, &crashed.runtime, &mut control));
    assert_eq!(failure.kind(), ErrorKind::Exited, "{failure}");
    assert_recorded(
        &failure,
        &crashed.lease,
        &locked,
        (Stage::Restore, Progress::Spawned),
        Evidence::LostStatus,
        Teardown::Terminated,
    );
    assert!(control.capabilities.is_empty());
    crashed.releases(&mut locked);
}

/// A launch that its caller abandons before the clone's activation leaves a progress log that
/// records the spawn of the clone's VMM, which the launch's journal, which the teardown removes,
/// records no more, so that a reattach reads the outcome report of the clone's VM, and records the
/// failure that the guest's status gives.
#[test]
fn a_launch_abandoned_before_its_activation_leaves_the_spawn_for_a_reattach() {
    let fixture = Fixture::new();
    complete_snapshot(&fixture);
    let (image, binding) = fixture.image("distro.erofs", IMAGE_LEN);
    let mut acquired = fixture.acquire("abandoned");
    let progress = acquired.locked.clone_progress_path();
    let launch = acquired.lease.launch().clone();
    let plan = fixture
        .plan(&mut acquired, &tmpfs_request(&image, &binding), NO_CONTENT)
        .unwrap();
    let runtime = plan.key().runtime().clone();
    let mut vmm = VmmDir::new();
    // The guest's repair ends the clone's VM with its status, which the VMM reports as it exits.
    vmm.plan("report:guest-exit:195");
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
    // The log records the spawn once the launch returns.
    let logged = read_progress(&progress, &launch).unwrap().unwrap();
    assert_eq!(
        (logged.progress, logged.boot),
        (Progress::Spawned, Some(BootId::current().unwrap()))
    );
    let capability = hex(restoring.capability());
    assert_eq!(vmm.started(), format!("{capability} true"));
    vmm.end_and_wait();
    assert_eq!(restoring.abandon().unwrap(), Recovery::Exited);
    assert!(acquired.locked.journal().unwrap().is_none());
    let mut control = ReattachControl::answering(Some);
    let reattached = reattach_clone(
        &mut acquired.locked,
        &runtime,
        &mut control,
        Instant::now() + TIMEOUT,
        TIMEOUT,
    );
    let failure = failed(reattached.unwrap());
    assert_recorded(
        &failure,
        &acquired.lease,
        &acquired.locked,
        (Stage::Restore, Progress::Spawned),
        Evidence::RepairStatus,
        Teardown::Exited,
    );
    assert_eq!(failure.record().status, Some(195));
    assert!(control.capabilities.is_empty());
    assert_eq!(
        fixture.store.release(&mut acquired.locked).unwrap(),
        Some(acquired.lease.clone())
    );
    assert_released(acquired.lease.sandbox());
}

#[test]
fn a_clone_whose_vm_exited_once_it_committed_is_classified_by_its_report() {
    let run = (Stage::Run, Progress::Committed);
    // A repair's status is evidence of the repair stage, after the clone's Ready too, and a VM
    // that ended without a report of the guest's own end lost the clone's status.
    let cases = [
        ("report:guest-exit:195", Evidence::RepairStatus, Some(195)),
        ("", Evidence::LostStatus, None),
    ];
    for (words, evidence, status) in cases {
        let crashed = Crashed::at_with("committed", words);
        crashed.vmm.end_and_wait();
        let mut locked = crashed.lock();
        let mut control = ReattachControl::answering(Some);
        let failure = failed(crashed.take_over(&mut locked, &crashed.runtime, &mut control));
        assert_eq!(failure.kind(), ErrorKind::Exited, "{words}: {failure}");
        let exited = Teardown::Exited;
        assert_recorded(&failure, &crashed.lease, &locked, run, evidence, exited);
        assert_eq!(failure.record().status, status, "{words}");
        assert!(locked.runtime().unwrap().is_none(), "{words}");
        crashed.releases(&mut locked);
    }
    // A VM that its guest ended with a status that is no evidence of a failure ended.
    let crashed = Crashed::at_with("committed", "report:guest-exit:3");
    crashed.vmm.end_and_wait();
    let mut locked = crashed.lock();
    let mut control = ReattachControl::answering(Some);
    match crashed.take_over(&mut locked, &crashed.runtime, &mut control) {
        ReattachedClone::Ended {
            progress: Progress::Committed,
            report: Some(report),
        } => {
            let outcome = (report.category.as_deref(), report.status_code);
            assert_eq!(outcome, (Some("guest-exit"), Some(3)));
        }
        other => panic!("the reattach found {}", describe(&other)),
    }
    // An inspection of the clone takes the report of its VM as the reattach did, and no failure.
    let records = CloneRecords::read(&locked).unwrap().unwrap();
    assert!(records.failure.is_none());
    let report = records.report.expect("the report of the clone's VM");
    let outcome = (report.category.as_deref(), report.status_code);
    assert_eq!(outcome, (Some("guest-exit"), Some(3)));
    crashed.releases(&mut locked);
}

#[test]
fn a_reattach_records_the_failure_of_a_vm_that_exited_before_it_forgets_the_vm() {
    let run = (Stage::Run, Progress::Committed);
    let crashed = Crashed::at_with("committed", "report:guest-exit:195");
    crashed.vmm.end_and_wait();
    let mut locked = crashed.lock();
    let mut control = ReattachControl::answering(Some);
    // The restarted caller crashes once its reattach cleared the runtime record of the VM that
    // exited, before it recorded the failure once more.
    let crash = reattach_clone_with(
        &mut locked,
        &crashed.runtime,
        &mut control,
        Instant::now() + TIMEOUT,
        TIMEOUT,
        |locked, runtime, exited| {
            supervisor::reattach_reporting(locked, runtime, exited)?;
            Err(Error::other("the test's caller crashes"))
        },
    );
    assert!(crash.is_err());
    // The sandbox recorded the failure before the runtime record went, with its teardown pending.
    let pending = read_failure(&locked.clone_failure_path())
        .unwrap()
        .expect("the reattach recorded the failure before it cleared the runtime record");
    assert_eq!(
        (pending.stage, pending.progress, pending.evidence),
        (run.0, run.1, Evidence::RepairStatus)
    );
    assert_eq!(pending.teardown, Teardown::Pending);
    assert_eq!(pending.status, Some(195));
    assert!(locked.runtime().unwrap().is_none());
    // The next reattach finishes it, with the status that it holds, even once no read finds the
    // report, as when another process holds it.
    fs::write(locked.outcome_path(), b"not a report").unwrap();
    let failure = failed(crashed.take_over(&mut locked, &crashed.runtime, &mut control));
    let (lease, exited) = (&crashed.lease, Teardown::Exited);
    assert_recorded(
        &failure,
        lease,
        &locked,
        run,
        Evidence::RepairStatus,
        exited,
    );
    assert_eq!(failure.record().status, Some(195));
    assert!(control.capabilities.is_empty());
    crashed.releases(&mut locked);
}

#[test]
fn a_reattach_takes_the_outcome_report_of_a_vm_that_exited_as_it_first_read_it() {
    let run = (Stage::Run, Progress::Committed);
    let crashed = Crashed::at_with("committed", "report:guest-exit:195");
    crashed.vmm.end_and_wait();
    let mut locked = crashed.lock();
    let outcome = locked.outcome_path();
    let mut control = ReattachControl::answering(Some);
    // Once the reattach read the report of the VM that exited, before it cleared the VM's runtime
    // record, no read finds the report, as when another process holds it: the reattach takes the
    // report as it first read it.
    let reattached = reattach_clone_with(
        &mut locked,
        &crashed.runtime,
        &mut control,
        Instant::now() + TIMEOUT,
        TIMEOUT,
        |locked, runtime, exited| {
            supervisor::reattach_reporting(locked, runtime, |record| {
                exited(record)?;
                fs::write(&outcome, b"not a report").unwrap();
                Ok(())
            })
        },
    )
    .unwrap();
    let failure = failed(reattached);
    let (lease, exited) = (&crashed.lease, Teardown::Exited);
    assert_recorded(
        &failure,
        lease,
        &locked,
        run,
        Evidence::RepairStatus,
        exited,
    );
    assert_eq!(failure.record().status, Some(195));
    crashed.releases(&mut locked);
}

/// Records the failure of the clone of `lease`, in the sandbox of `locked`, at `at`, with
/// `evidence` and `error`, whose teardown is `teardown`, with the transaction `transaction`, as
/// a failure of the clone's restore sequence records it before its teardown, which a crash of its
/// caller interrupted, if the teardown is pending, or once a teardown that could not end the
/// clone's VMM, if it is unresolved.
fn record_earlier(
    lease: &Lease,
    locked: &LockedStore,
    (at, transaction): ((Stage, Progress), Option<TransactionId>),
    (evidence, teardown): (Evidence, Teardown),
    error: &Error,
) {
    let record = FailureRecord::new(lease, transaction, at, evidence, None, teardown, error);
    write_failure(&locked.clone_failure_path(), &record).unwrap();
}

/// Returns the transaction of the restore that the activation record of the clone in the sandbox
/// of `locked` names.
fn transaction_of(locked: &LockedStore) -> Option<TransactionId> {
    let activation = fs::read(locked.clone_activation_path()).unwrap();
    let activation = ActivationRecord::decode(&activation).unwrap();
    activation.restore().map(|restore| restore.transaction)
}

#[test]
fn a_reattach_finishes_an_earlier_failure_with_what_the_clone_s_vmm_left_as_it_ended() {
    let restore = (Stage::Restore, Progress::Spawned);
    let mut control = ReattachControl::answering(Some);
    let unresolved = Teardown::Unresolved("the test could not end the clone's VMM".to_owned());
    for earlier in [Teardown::Pending, unresolved] {
        // A failure of the clone's restore sequence, whose teardown a crash of its caller
        // interrupted, or could not end the clone's VMM, which the guest then ended with the
        // status of a failed repair, which prevails.
        let crashed = Crashed::at_with("launched", "report:guest-exit:195");
        let mut locked = crashed.lock();
        let reset = Error::protocol("the test's clone reset its session");
        let at = (restore, transaction_of(&locked));
        let teardown = earlier.clone();
        record_earlier(
            &crashed.lease,
            &locked,
            at,
            (Evidence::Reset, teardown),
            &reset,
        );
        crashed.vmm.end_and_wait();
        let failure = failed(crashed.take_over(&mut locked, &crashed.runtime, &mut control));
        assert_eq!(
            failure.kind(),
            ErrorKind::Protocol,
            "{earlier:?}: {failure}"
        );
        let (lease, exited) = (&crashed.lease, Teardown::Exited);
        assert_recorded(
            &failure,
            lease,
            &locked,
            restore,
            Evidence::RepairStatus,
            exited,
        );
        assert_eq!(failure.record().status, Some(195), "{earlier:?}");
        assert_eq!(failure.message(), "the test's clone reset its session");
        crashed.releases(&mut locked);
        // One of the template, such as a defect of the generation that the caller diagnosed,
        // which the status does not override: the record holds the status beside it.
        let crashed = Crashed::at_with("launched", "report:guest-exit:195");
        let mut locked = crashed.lock();
        let defect = Error::other("the test diagnosed a defect of the generation's repair");
        let at = (restore, transaction_of(&locked));
        let teardown = earlier.clone();
        record_earlier(
            &crashed.lease,
            &locked,
            at,
            (Evidence::TemplateDefect, teardown),
            &defect,
        );
        crashed.vmm.end_and_wait();
        let failure = failed(crashed.take_over(&mut locked, &crashed.runtime, &mut control));
        let (lease, template) = (&crashed.lease, Evidence::TemplateDefect);
        assert_recorded(
            &failure,
            lease,
            &locked,
            restore,
            template,
            Teardown::Exited,
        );
        assert_eq!(
            (failure.outcome(), failure.record().status),
            (Outcome::Template, Some(195)),
            "{earlier:?}"
        );
        crashed.releases(&mut locked);
        // One whose VMM exited before its restore-ready event, with OpenVMM's error, which tells
        // that this host cannot present the template's CPU, once its record was written.
        let crashed = Crashed::at_with("launched", "close fatal:E_CPU_GENERATION");
        let mut locked = crashed.lock();
        let lost = Error::exited("the test's VMM exited before its restore-ready event");
        let at = (restore, transaction_of(&locked));
        let teardown = earlier.clone();
        record_earlier(
            &crashed.lease,
            &locked,
            at,
            (Evidence::LostStatus, teardown),
            &lost,
        );
        crashed.vmm.wait_exit();
        let failure = failed(crashed.take_over(&mut locked, &crashed.runtime, &mut control));
        assert_eq!(failure.kind(), ErrorKind::Exited, "{earlier:?}: {failure}");
        let (lease, incompatible) = (&crashed.lease, Evidence::IncompatibleHost);
        assert_recorded(
            &failure,
            lease,
            &locked,
            restore,
            incompatible,
            Teardown::Exited,
        );
        let message = failure.message();
        assert!(
            message.starts_with("the test's VMM exited before its restore-ready event; ")
                && message.ends_with("failed to launch vm worker")
                && message.matches("[E_CPU_GENERATION]").count() == 1,
            "{earlier:?}: {failure}"
        );
        // A later reattach finds the failure as it was recorded.
        let again = failed(crashed.take_over(&mut locked, &crashed.runtime, &mut control));
        assert_eq!(again.record(), failure.record(), "{earlier:?}");
        crashed.releases(&mut locked);
    }
    assert!(control.capabilities.is_empty());
    // The record keeps the transaction of the restore that failed, whatever the sandbox's
    // activation record names since.
    let crashed = Crashed::at_with("launched", "report:guest-exit:195");
    let mut locked = crashed.lock();
    let reset = Error::protocol("the test's clone reset its session");
    let transaction = Some(TransactionId::from_bytes([0x5a; 16]));
    assert_ne!(transaction, transaction_of(&locked));
    let pending = (Evidence::Reset, Teardown::Pending);
    record_earlier(
        &crashed.lease,
        &locked,
        (restore, transaction),
        pending,
        &reset,
    );
    crashed.vmm.end_and_wait();
    let failure = failed(crashed.take_over(&mut locked, &crashed.runtime, &mut control));
    let record = failure.record();
    assert_eq!(
        (record.transaction, record.evidence, &record.teardown),
        (transaction, Evidence::RepairStatus, &Teardown::Exited)
    );
    let recorded = read_failure(&locked.clone_failure_path()).unwrap();
    assert_eq!(recorded.as_ref(), Some(record));
    crashed.releases(&mut locked);
}

#[test]
fn a_clone_whose_progress_log_may_have_lost_its_steps_may_have_run_its_workload() {
    // A launch whose caller crashed once the clone was ready, or once it sent the start of the
    // clone's sandbox, whose VM the guest ended with a status that the clone's root gives before
    // that start, and a workload may give after it.
    let (current, rebooted) = (BootId::current().unwrap(), BootId::from_bytes([0xee; 16]));
    assert_ne!(current, rebooted);
    let ready = (Stage::Start, Progress::Ready);
    let spawned = (Stage::Restore, Progress::Spawned);
    let cases = [
        // Its intact log tells that the clone was only ready: its root's status is evidence.
        ("intact", "activated", ready, Evidence::UnusableRoot),
        // A log that was lost, or damaged, tells nothing, and one that the launch wrote during
        // another boot of the host may have lost the steps of the start with the host's crash:
        // the status is no evidence of the root's failure, and the clone lost its status.
        ("lost", "started", spawned, Evidence::LostStatus),
        ("damaged", "started", spawned, Evidence::LostStatus),
        ("rebooted", "started", ready, Evidence::LostStatus),
    ];
    for (damage, step, at, evidence) in cases {
        let crashed = Crashed::at_with(step, "report:guest-exit:54");
        let mut locked = crashed.lock();
        let log = locked.clone_progress_path();
        let launch = crashed.lease.launch();
        match damage {
            "lost" => fs::remove_file(log).unwrap(),
            "damaged" => fs::write(log, b"not a progress log\n").unwrap(),
            "rebooted" => fs::write(
                log,
                format!(
                    "nvx-clone-progress 1 {launch} {rebooted}\nspawned\nevent\nacknowledged\n\
                     activating\nready\n"
                ),
            )
            .unwrap(),
            _ => {}
        }
        crashed.vmm.end_and_wait();
        let mut control = ReattachControl::answering(Some);
        let failure = failed(crashed.take_over(&mut locked, &crashed.runtime, &mut control));
        let record = failure.record();
        assert_eq!(
            (
                (record.stage, record.progress),
                record.evidence,
                record.status,
                &record.teardown
            ),
            (at, evidence, Some(54), &Teardown::Exited),
            "{damage}: {failure}"
        );
        let recorded = read_failure(&locked.clone_failure_path()).unwrap();
        assert_eq!(recorded.as_ref(), Some(record), "{damage}");
        crashed.releases(&mut locked);
    }
}

#[test]
fn a_clone_that_its_holder_stopped_ended_whatever_a_reboot_left_of_its_progress_log() {
    let other = BootId::from_bytes([0xee; 16]);
    assert_ne!(other, BootId::current().unwrap());
    for rebooted in [false, true] {
        let crashed = Crashed::at_with("committed", "report:guest-exit:54");
        let mut locked = crashed.lock();
        let mut control = ReattachControl::answering(Some);
        let vmm = match crashed.take_over(&mut locked, &crashed.runtime, &mut control) {
            ReattachedClone::Live { vmm, .. } => vmm,
            other => panic!("the reattach found {}", describe(&other)),
        };
        // Its holder stops the clone, whose workload ends with a status that the clone's root
        // gives before the start of its sandbox, which clears the store's records of its VM.
        let stopped = vmm
            .stop(&mut locked, Instant::now() + TIMEOUT, None, |_, _| {
                crashed.vmm.end();
                Ok::<(), io::Error>(())
            })
            .unwrap();
        assert_eq!(stopped.outcome, StopOutcome::Graceful, "{rebooted}");
        assert!(locked.runtime().unwrap().is_none(), "{rebooted}");
        if rebooted {
            // A crash of the host, before the release, lost the last lines of the progress log,
            // which the launch wrote during the boot that the crash ended.
            let launch = crashed.lease.launch();
            fs::write(
                locked.clone_progress_path(),
                format!(
                    "nvx-clone-progress 1 {launch} {other}\nspawned\nevent\nacknowledged\n\
                     activating\nready\n"
                ),
            )
            .unwrap();
        }
        // The clone ended, and its workload's status is no failure of its root.
        let progress = if rebooted {
            Progress::Ready
        } else {
            Progress::Committed
        };
        match crashed.take_over(&mut locked, &crashed.runtime, &mut control) {
            ReattachedClone::Ended {
                progress: ended,
                report: Some(report),
            } if ended == progress => {
                let outcome = (report.category.as_deref(), report.status_code);
                assert_eq!(outcome, (Some("guest-exit"), Some(54)), "{rebooted}");
            }
            other => panic!("{rebooted}: the reattach found {}", describe(&other)),
        }
        let recorded = read_failure(&locked.clone_failure_path()).unwrap();
        assert!(recorded.is_none(), "{rebooted}: {recorded:?}");
        crashed.releases(&mut locked);
    }
}

#[test]
fn a_clone_whose_vmm_exited_before_its_holder_s_stop_began_lost_its_status() {
    let never = |_: &Vmm, _: Instant| -> io::Result<()> {
        panic!("a stop ran the graceful step of a VMM that had exited")
    };
    // Its holder found the clone's VMM running, which then exits, without a report of the
    // guest's own end, before the holder's stop begins.
    let crashed = Crashed::at("committed");
    let mut locked = crashed.lock();
    let mut control = ReattachControl::answering(Some);
    let vmm = match crashed.take_over(&mut locked, &crashed.runtime, &mut control) {
        ReattachedClone::Live { vmm, .. } => vmm,
        other => panic!("the reattach found {}", describe(&other)),
    };
    crashed.vmm.end_and_wait();
    let stopped = vmm
        .stop_unless_exited(&mut locked, Instant::now() + TIMEOUT, None, never)
        .unwrap();
    assert!(stopped.is_none(), "{stopped:?}");
    // The stop kept the runtime record, and with it the lease, so the reattach finds that the VM
    // exited unheld: the clone lost its status, which quarantines nothing.
    assert_eq!(locked.runtime().unwrap().as_ref(), Some(vmm.record()));
    crashed.refuses_release(&mut locked);
    let failure = failed(crashed.take_over(&mut locked, &crashed.runtime, &mut control));
    assert_eq!(failure.kind(), ErrorKind::Exited, "{failure}");
    let (run, lost) = ((Stage::Run, Progress::Committed), Evidence::LostStatus);
    assert_recorded(
        &failure,
        &crashed.lease,
        &locked,
        run,
        lost,
        Teardown::Exited,
    );
    assert_eq!(failure.record().status, None);
    let deadline = Some(Instant::now() + TIMEOUT);
    assert!(
        !failure
            .quarantine(&crashed.fixture.store, deadline)
            .unwrap()
    );
    assert!(locked.runtime().unwrap().is_none());
    crashed.releases(&mut locked);

    // A stop that ends such a VM as one that it stopped clears the record, and the clone ended.
    let crashed = Crashed::at("committed");
    let mut locked = crashed.lock();
    let vmm = match crashed.take_over(&mut locked, &crashed.runtime, &mut control) {
        ReattachedClone::Live { vmm, .. } => vmm,
        other => panic!("the reattach found {}", describe(&other)),
    };
    crashed.vmm.end_and_wait();
    let stopped = vmm
        .stop(&mut locked, Instant::now() + TIMEOUT, None, never)
        .unwrap();
    assert_eq!(stopped.outcome, StopOutcome::Graceful);
    assert!(locked.runtime().unwrap().is_none());
    match crashed.take_over(&mut locked, &crashed.runtime, &mut control) {
        ReattachedClone::Ended {
            progress: Progress::Committed,
            report: None,
        } => {}
        other => panic!("the reattach found {}", describe(&other)),
    }
    let recorded = read_failure(&locked.clone_failure_path()).unwrap();
    assert!(recorded.is_none(), "{recorded:?}");
    crashed.releases(&mut locked);
}

#[test]
fn an_interrupted_launch_that_a_reattach_cannot_end_yet_keeps_its_lease_until_one_can() {
    let crashed = Crashed::at("activated");
    let mut locked = crashed.lock();
    let mut control = ReattachControl::answering(Some);
    // The reattach cannot tell whether the launch's VMM still runs.
    let reason = "the test cannot end the launch yet";
    let reattached = reattach_clone_with(
        &mut locked,
        &crashed.runtime,
        &mut control,
        Instant::now() + TIMEOUT,
        TIMEOUT,
        |_, _, _| Ok(Reattached::Indeterminate(reason.to_owned())),
    )
    .unwrap();
    let failure = failed(reattached);
    let ready = (Stage::Start, Progress::Ready);
    let unresolved = Teardown::Unresolved(reason.to_owned());
    let lost = Evidence::LostStatus;
    assert_recorded(&failure, &crashed.lease, &locked, ready, lost, unresolved);
    assert!(failure.message().ends_with(reason), "{failure}");
    // The journal remains, and with it the lease, and the clone's records.
    assert!(locked.journal().unwrap().is_some());
    crashed.refuses_release(&mut locked);
    // A reattach that can end the launch resolves the teardown, and keeps the failure as it was.
    let resolved = failed(crashed.take_over(&mut locked, &crashed.runtime, &mut control));
    let terminated = Teardown::Terminated;
    assert_recorded(&resolved, &crashed.lease, &locked, ready, lost, terminated);
    assert_eq!(resolved.message(), failure.record().message);
    assert!(control.capabilities.is_empty());
    crashed.releases(&mut locked);
}

/// Returns the error of `reattached`, which must have failed.
fn reattach_error<S>(reattached: crate::Result<ReattachedClone<S>>) -> Error {
    match reattached {
        Err(error) => error,
        Ok(other) => panic!("the reattach found {}", describe(&other)),
    }
}

/// Takes the clone of the sandbox of `locked` over, under `runtime`, through `control`, with
/// [`reattach_clone`], while a directory takes the place of the record of the clone's failure, as
/// on a storage that refuses to write it, once the reattach read the sandbox's records, and
/// returns the error of the reattach.
fn unrecorded_take_over(
    locked: &mut LockedStore,
    runtime: &RuntimeIdentity,
    control: &mut ReattachControl,
) -> Error {
    let path = locked.clone_failure_path();
    let reattached = reattach_clone_with(
        locked,
        runtime,
        control,
        Instant::now() + TIMEOUT,
        TIMEOUT,
        |locked, runtime, exited| {
            fs::create_dir(&path).unwrap();
            supervisor::reattach_reporting(locked, runtime, exited)
        },
    );
    fs::remove_dir(&path).unwrap();
    reattach_error(reattached)
}

#[test]
fn a_reattach_that_cannot_record_a_failure_ends_and_forgets_nothing_of_the_clone() {
    let unrecorded = |error: &Error| {
        let left = "; the reattach left the clone's VM and its records as it found them, for a \
                    later reattach";
        let message = error.message();
        assert_eq!(error.kind(), ErrorKind::Io, "{error}");
        let start = "the failure could not be recorded before its teardown: ";
        assert!(
            message.starts_with(start) && message.ends_with(left),
            "{error}"
        );
    };
    let mut control = ReattachControl::answering(Some);
    // A record of a failure that cannot be read, which the reattach would replace with its own
    // before it ends the interrupted launch that the journal names.
    let crashed = Crashed::at("activated");
    let mut locked = crashed.lock();
    let path = locked.clone_failure_path();
    fs::create_dir(&path).unwrap();
    let deadline = Instant::now() + TIMEOUT;
    let runtime = &crashed.runtime;
    let reattached = reattach_clone(&mut locked, runtime, &mut control, deadline, TIMEOUT);
    unrecorded(&reattach_error(reattached));
    let journal = locked
        .journal()
        .unwrap()
        .expect("the reattach kept the journal");
    assert!(Process::open(journal.process().unwrap()).unwrap().is_some());
    crashed.refuses_release(&mut locked);
    // Once the record can be written, the reattach ends the launch, and records its failure.
    fs::remove_dir(&path).unwrap();
    let failure = failed(crashed.take_over(&mut locked, runtime, &mut control));
    let (ready, killed) = ((Stage::Start, Progress::Ready), Evidence::ForcedKill);
    let terminated = Teardown::Terminated;
    assert_recorded(&failure, &crashed.lease, &locked, ready, killed, terminated);
    crashed.releases(&mut locked);
    // A VM whose launch committed, and that exited without a report of the guest's own end: the
    // reattach would record that the clone lost its status before it clears the VM's record.
    let crashed = Crashed::at("committed");
    crashed.vmm.end_and_wait();
    let mut locked = crashed.lock();
    let record = locked.runtime().unwrap();
    assert!(record.is_some());
    unrecorded(&unrecorded_take_over(
        &mut locked,
        &crashed.runtime,
        &mut control,
    ));
    assert_eq!(locked.runtime().unwrap(), record);
    crashed.refuses_release(&mut locked);
    let failure = failed(crashed.take_over(&mut locked, &crashed.runtime, &mut control));
    let (run, lost) = ((Stage::Run, Progress::Committed), Evidence::LostStatus);
    assert_recorded(
        &failure,
        &crashed.lease,
        &locked,
        run,
        lost,
        Teardown::Exited,
    );
    crashed.releases(&mut locked);
    // A VM that runs under another runtime identity, and one whose Ready reports another
    // activation than its record's: the reattach would end either once it recorded why.
    let other = RuntimeIdentity::new(artifact('d'), artifact('e'), artifact('f'));
    let mut refusing = ReattachControl::answering(|_| Some(ReadyActivation { transaction: None }));
    let reattach = (Stage::Reattach, Progress::Committed);
    let cases = [
        (Some(&other), Evidence::RuntimeMismatch),
        (None, Evidence::Protocol),
    ];
    for (runtime, evidence) in cases {
        let crashed = Crashed::at("committed");
        let mut locked = crashed.lock();
        let (runtime, control) = match runtime {
            Some(other) => (other, &mut control),
            None => (&crashed.runtime, &mut refusing),
        };
        unrecorded(&unrecorded_take_over(&mut locked, runtime, control));
        let record = locked
            .runtime()
            .unwrap()
            .expect("the reattach kept the record");
        assert!(
            Process::open(record.process()).unwrap().is_some(),
            "{evidence:?}"
        );
        crashed.refuses_release(&mut locked);
        let failure = failed(crashed.take_over(&mut locked, runtime, control));
        let terminated = Teardown::Terminated;
        assert_recorded(
            &failure,
            &crashed.lease,
            &locked,
            reattach,
            evidence,
            terminated,
        );
        crashed.releases(&mut locked);
    }
}

#[test]
fn a_reattach_makes_the_record_of_an_earlier_failure_durable_before_it_ends_the_launch() {
    // A failure of the clone's launch, whose record is in place, but whose teardown a crash of
    // its caller interrupted, maybe once the sync of the record's directory failed.
    let crashed = Crashed::at("activated");
    let mut locked = crashed.lock();
    let ready = (Stage::Start, Progress::Ready);
    let error = Error::other("the test's launch failed");
    let at = (ready, transaction_of(&locked));
    let earlier = (Evidence::Protocol, Teardown::Pending);
    record_earlier(&crashed.lease, &locked, at, earlier, &error);
    let mut control = ReattachControl::answering(Some);
    let reattach = |locked: &mut LockedStore,
                    runtime: &RuntimeIdentity,
                    exited: &dyn Fn(&RuntimeRecord) -> Result<()>| {
        supervisor::reattach_reporting(locked, runtime, exited)
    };
    // A reattach that cannot make the record durable ends nothing.
    let refused = |_: &Path| -> Result<()> { Err(Error::io("the test's sync failed")) };
    let deadline = Instant::now() + TIMEOUT;
    let runtime = &crashed.runtime;
    let reattached = reattach_clone_syncing(
        &mut locked,
        runtime,
        &mut control,
        deadline,
        TIMEOUT,
        &refused,
        reattach,
    );
    let error = reattach_error(reattached);
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    let refusal = "could not be made durable before its teardown: the test's sync failed";
    assert!(error.message().contains(refusal), "{error}");
    let journal = locked
        .journal()
        .unwrap()
        .expect("the reattach kept the journal");
    assert!(Process::open(journal.process().unwrap()).unwrap().is_some());
    crashed.refuses_release(&mut locked);
    // One that can syncs the sandbox's directory before it ends the launch.
    let synced = RefCell::new(Vec::new());
    let sync = |dir: &Path| -> Result<()> {
        synced.borrow_mut().push(dir.to_owned());
        sync_dir(dir)
    };
    let dir = locked.dir().to_owned();
    let reattached = reattach_clone_syncing(
        &mut locked,
        runtime,
        &mut control,
        Instant::now() + TIMEOUT,
        TIMEOUT,
        &sync,
        |locked, runtime, exited| {
            assert_eq!(*synced.borrow(), std::slice::from_ref(&dir));
            reattach(locked, runtime, exited)
        },
    );
    let failure = failed(reattached.unwrap());
    let (protocol, terminated) = (Evidence::Protocol, Teardown::Terminated);
    assert_recorded(
        &failure,
        &crashed.lease,
        &locked,
        ready,
        protocol,
        terminated,
    );
    crashed.releases(&mut locked);
}
