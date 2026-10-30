//! What the VM tests share: the sandbox of a test, in which they run the host program's commands,
//! the fixtures of its layered roots, and the environment that they read.
//!
//! The tests read:
//!
//! - `NVX_HYPERVISOR`: the hypervisor, through `nvx::Hypervisor::from_env_or_default`;
//! - `NVX_ARTIFACTS_DIR`: NVX's artifacts, a repository checkout or an extracted release, with
//!   the install metadata from which NVX gives their runtime identity, and the [`Fixtures`] of
//!   the layered roots;
//! - `RUNTIME_ADAPTER_GUEST`: the guest agent, a static `x86_64-unknown-linux-musl` build of
//!   `runtime-adapter-guest`.
// Each test crate uses its own part of this module.
#![allow(dead_code)]

pub(crate) mod gpt;

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::env;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use nvx::Hypervisor;
use nvx::host::artifacts::{ArtifactDir, Artifacts, Guest};
use nvx::host::process::Process;
use nvx::host::store::{LOG_NAME, RuntimeRecord, SandboxStore};
use nvx::host::supervisor::{self, Reattached, StopOutcome};
use nvx::host::template::{CAPTURES_NAME, Store, TRANSACTION_NAME};
use runtime_adapter::layered::RECORD_NAME;
use serde_json::Value;

/// The variable that names the guest agent.
pub(crate) const GUEST_AGENT: &str = "RUNTIME_ADAPTER_GUEST";
/// The scratch template among NVX's artifacts.
pub(crate) const SCRATCH_TEMPLATE: &str = "ubuntu-smoke-scratch.ext4";
/// The longest that a command of the host program may take.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(180);
/// How much of the end of the VMM's log a failure shows.
const LOG_TAIL: usize = 8192;

/// The `KEY=VALUE` lines that a command of the host program printed.
pub(crate) type Printed = BTreeMap<String, String>;

/// The fixtures of the layered roots, those of NVX's own layered scenarios, among NVX's artifacts:
///
/// - the Ubuntu EROFS image, `ubuntu-distro.erofs`, and its manifest, which holds its UUID;
/// - the scratch template, [`SCRATCH_TEMPLATE`], an empty ext4 file system that NVX's packages
///   ship with the guest artifacts, of which each test boots a copy.
pub(crate) struct Fixtures {
    /// NVX's Ubuntu EROFS image.
    pub(crate) distro: PathBuf,
    /// The image's manifest, beside it.
    pub(crate) manifest: PathBuf,
    /// The image's UUID, from its manifest.
    pub(crate) identity: String,
    /// The scratch template.
    pub(crate) scratch: PathBuf,
}

impl Fixtures {
    /// Finds the fixtures among NVX's artifacts in `NVX_ARTIFACTS_DIR`.
    pub(crate) fn find() -> Self {
        let directory = env::var_os(Artifacts::ENV_DIR)
            .unwrap_or_else(|| panic!("{} must name NVX's artifacts", Artifacts::ENV_DIR));
        let artifacts = ArtifactDir::from_dir(directory).unwrap();
        let distro = artifacts.distro_layer(Guest::Ubuntu).unwrap();
        let manifest = artifacts.distro_manifest(Guest::Ubuntu).unwrap();
        let scratch = artifacts.artifact(SCRATCH_TEMPLATE);
        for (path, fixture) in [
            (&distro, "the Ubuntu EROFS image"),
            (&manifest, "the manifest of the Ubuntu EROFS image"),
            (
                &scratch,
                "the scratch template, an empty ext4 file system, which `nvx-rs download` installs",
            ),
        ] {
            assert!(path.is_file(), "{fixture} is missing: {}", path.display());
        }
        let contents: Value = serde_json::from_str(&fs::read_to_string(&manifest).unwrap())
            .expect("the manifest is JSON");
        let identity = contents["uuid"]
            .as_str()
            .expect("the manifest holds the image's UUID")
            .to_owned();
        Self {
            distro,
            manifest,
            identity,
            scratch,
        }
    }
}

/// A sandbox of a test: a directory of its own, which holds the store of its commands, the output
/// of each command of the host program that the test runs, and any file that the test makes for
/// its VM.
///
/// Dropping it ends a VM that a failed test left running, through its verified identity, and
/// removes the directory once no VM of the sandbox runs. Should that remain uncertain, it keeps
/// the directory, whose store a later reattach needs to end the VM, and prints its path.
pub(crate) struct Sandbox {
    /// The test's directory.
    pub(crate) root: PathBuf,
    /// The store of the commands that the test runs: the sandbox's store, which the launch
    /// creates, or a template store, which `create` or `refresh` creates, whose captures run their
    /// VMs in its slots.
    pub(crate) store: PathBuf,
    /// The number of commands that the test ran.
    commands: usize,
    /// The output files of the last command that the test ran, without their extensions.
    output: Option<PathBuf>,
}

/// A command of the host program that a test started, and that may still run, as one that the
/// step hook paused, until [`Sandbox::finish`] waits for it to end.
///
/// Dropping it ends a command that still runs, as when a test fails while the hook pauses the
/// command, so that the sandbox's own drop can take the sandbox's lock, which the command holds,
/// and end the VM.
pub(crate) struct Started {
    /// The command.
    command: String,
    /// Its options.
    options: Vec<OsString>,
    /// Its process.
    child: Child,
    /// Its output files, without their extensions.
    output: PathBuf,
    /// When it must have ended.
    deadline: Instant,
}

impl Drop for Started {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

impl Sandbox {
    /// Creates the directory of the test `name`, in the system's temporary directory, so that the
    /// path of a Linux control socket in it stays short. A directory that an earlier test process
    /// kept stays as it is.
    pub(crate) fn new(name: &str) -> Self {
        let pid = std::process::id();
        let root = (0..)
            .map(|attempt| env::temp_dir().join(format!("runtime-adapter-{name}-{pid}-{attempt}")))
            .find(|root| match fs::create_dir(root) {
                Ok(()) => true,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
                Err(error) => panic!("cannot create {}: {error}", root.display()),
            })
            .expect("a directory name is free");
        Self {
            store: root.join("store"),
            root,
            commands: 0,
            output: None,
        }
    }

    /// Launches the VM with the guest agent, on the hypervisor that `NVX_HYPERVISOR` selects, and
    /// with `options` too, checks that the launch exits with `status`, and returns what it
    /// printed. A launch that succeeded recorded its VM, which runs.
    pub(crate) fn launch(&mut self, options: &[&OsStr], status: i32) -> Printed {
        let started = self.start_launch(options, &[]);
        self.finish_launch(started, status)
    }

    /// Starts a launch of the VM, as [`launch`](Self::launch) does, with the variables
    /// `environment` too, and returns it while it runs.
    pub(crate) fn start_launch(
        &mut self,
        options: &[&OsStr],
        environment: &[(&str, &OsStr)],
    ) -> Started {
        let guest = guest_options();
        let mut arguments: Vec<&OsStr> = guest.iter().map(OsString::as_os_str).collect();
        arguments.extend(options);
        self.start("launch", &arguments, environment)
    }

    /// Creates a template of the guest agent in the sandbox's store, a template store, on the
    /// hypervisor that `NVX_HYPERVISOR` selects, and with `options` too, checks that the creation
    /// exits with `status`, and returns what it printed.
    pub(crate) fn create(&mut self, options: &[&OsStr], status: i32) -> Printed {
        let guest = guest_options();
        let mut arguments: Vec<&OsStr> = guest.iter().map(OsString::as_os_str).collect();
        arguments.extend(options);
        let started = self.start("create", &arguments, &[]);
        self.finish(started, status)
    }

    /// Refreshes a template of the guest agent in the sandbox's store, a template store, as
    /// [`create`](Self::create) creates one, with `options` too, checks that the refresh exits
    /// with `status`, and returns what it printed.
    pub(crate) fn refresh(&mut self, options: &[&OsStr], status: i32) -> Printed {
        let guest = guest_options();
        let mut arguments: Vec<&OsStr> = guest.iter().map(OsString::as_os_str).collect();
        arguments.extend(options);
        let started = self.start("refresh", &arguments, &[]);
        self.finish(started, status)
    }

    /// Waits for the launch `started` to end, checks that it exits with `status`, and returns what
    /// it printed. A launch that succeeded recorded its VM, which runs.
    pub(crate) fn finish_launch(&self, started: Started, status: i32) -> Printed {
        let launched = self.finish(started, status);
        if status == 0 {
            let record = self.record().expect("the launch recorded its VM");
            assert_eq!(launched["pid"], record.process().pid.to_string());
        }
        launched
    }

    /// Runs the host program's `command` on the sandbox's store with `options`, checks that it
    /// exits with `status`, and returns the `KEY=VALUE` lines that it printed.
    pub(crate) fn run(&mut self, command: &str, options: &[&OsStr], status: i32) -> Printed {
        let started = self.start(command, options, &[]);
        self.finish(started, status)
    }

    /// Starts the host program's `command` on the sandbox's store with `options`, and with the
    /// variables `environment` too, and returns it while it runs.
    pub(crate) fn start(
        &mut self,
        command: &str,
        options: &[&OsStr],
        environment: &[(&str, &OsStr)],
    ) -> Started {
        self.commands += 1;
        let output = self.root.join(format!("{}-{command}", self.commands));
        self.output = Some(output.clone());
        let (stdout, stderr) = (output.with_extension("out"), output.with_extension("err"));
        // Files, rather than pipes, take the output, so that nothing waits for a VMM that might
        // hold them. A test may launch a VM in its own process meanwhile, so the command starts
        // under a `SpawnLock`, which keeps it from inheriting that VMM's handles on Windows, such
        // as its log and its capability's pipe, which it would hold until it exits.
        let mut host = Command::new(env!("CARGO_BIN_EXE_runtime-adapter-host"));
        host.arg(command)
            .arg(&self.store)
            .args(options)
            .envs(environment.iter().copied())
            .stdin(Stdio::null())
            .stdout(File::create(&stdout).unwrap())
            .stderr(File::create(&stderr).unwrap());
        let child = supervisor::spawn_command(&mut host).unwrap();
        Started {
            command: command.to_owned(),
            options: options.iter().map(|option| option.to_os_string()).collect(),
            child,
            output,
            deadline: Instant::now() + COMMAND_TIMEOUT,
        }
    }

    /// Waits for the command `started` to end, checks that it exits with `status`, and returns the
    /// `KEY=VALUE` lines that it printed.
    pub(crate) fn finish(&self, mut started: Started, status: i32) -> Printed {
        let exit = loop {
            if let Some(exit) = started.child.try_wait().unwrap() {
                break exit;
            }
            self.check_deadline(&mut started);
            thread::sleep(Duration::from_millis(50));
        };
        let command = &started.command;
        assert_eq!(
            exit.code(),
            Some(status),
            "{command} {:?} ended with {exit}{}",
            started.options,
            self.diagnostics(&started.output)
        );
        let printed = fs::read_to_string(started.output.with_extension("out")).unwrap();
        let mut lines = BTreeMap::new();
        for line in printed.lines() {
            let (key, value) = line
                .split_once('=')
                .unwrap_or_else(|| panic!("{command} printed {line:?}"));
            let earlier = lines.insert(key.to_owned(), value.to_owned());
            assert!(earlier.is_none(), "{command} printed {key} twice");
        }
        lines
    }

    /// Waits until the command `started` creates the file at `path` with the name of a step, as
    /// the host program's step hook does when it pauses the command, and returns that name. Fails
    /// if the command ends first.
    pub(crate) fn paused(&self, started: &mut Started, path: &Path) -> String {
        loop {
            match fs::read_to_string(path) {
                Ok(step) if !step.is_empty() => return step,
                _ => {}
            }
            if let Some(exit) = started.child.try_wait().unwrap() {
                panic!(
                    "{} ended with {exit} before it paused{}",
                    started.command,
                    self.diagnostics(&started.output)
                );
            }
            self.check_deadline(started);
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Ends the command `started`, and fails, once its deadline has passed.
    fn check_deadline(&self, started: &mut Started) {
        if Instant::now() >= started.deadline {
            let _ = started.child.kill();
            let _ = started.child.wait();
            panic!(
                "{} did not end in time{}",
                started.command,
                self.diagnostics(&started.output)
            );
        }
    }

    /// Returns what the command whose output files start with `output` printed, and the end of the
    /// VMM's log, or of the log of each capture's VMM in a template store.
    fn diagnostics(&self, output: &Path) -> String {
        let read = |path: PathBuf| {
            let bytes = fs::read(path).unwrap_or_default();
            let tail = bytes.len().saturating_sub(LOG_TAIL);
            String::from_utf8_lossy(&bytes[tail..]).into_owned()
        };
        let mut diagnostics = format!(
            "\nstdout:\n{}\nstderr:\n{}\nthe end of the VMM's log:\n{}",
            read(output.with_extension("out")),
            read(output.with_extension("err")),
            read(self.store.join(LOG_NAME)),
        );
        let slots = fs::read_dir(self.store.join(CAPTURES_NAME))
            .into_iter()
            .flatten();
        for slot in slots.flatten() {
            // The slot keeps the log of its latest capture once the capture's launch has ended,
            // and until then the capture's transaction does.
            let logs = [
                slot.path().join(LOG_NAME),
                slot.path().join(TRANSACTION_NAME).join(LOG_NAME),
            ];
            for log in logs.into_iter().filter(|log| log.exists()) {
                diagnostics.push_str(&format!(
                    "\nthe end of the capture's VMM's log {}:\n{}",
                    log.display(),
                    read(log.clone())
                ));
            }
        }
        diagnostics
    }

    /// Returns what the last command that the test ran wrote to its standard error.
    pub(crate) fn stderr(&self) -> String {
        let output = self.output.as_ref().expect("the test ran a command");
        fs::read_to_string(output.with_extension("err")).unwrap()
    }

    /// Checks that no launch of the sandbox got as far as spawning a VMM: its store holds no
    /// runtime record, no launch journal, no VMM's log, and no record of a launch of the host
    /// program, which a launch writes just before it spawns the VMM.
    pub(crate) fn assert_unlaunched(&self) {
        let store = SandboxStore::open(&self.store).unwrap();
        assert_eq!(
            store.runtime().unwrap(),
            None,
            "a runtime record names a VM"
        );
        assert!(store.journal().unwrap().is_none(), "a launch was begun");
        for name in [LOG_NAME, RECORD_NAME] {
            let path = self.store.join(name);
            assert!(!path.exists(), "{} exists", path.display());
        }
    }

    /// Returns the sandbox's runtime record, if it has one.
    pub(crate) fn record(&self) -> Option<RuntimeRecord> {
        SandboxStore::open(&self.store).unwrap().runtime().unwrap()
    }

    /// Returns the VMM of the sandbox's runtime record, if the record names one that runs.
    pub(crate) fn vmm(&self) -> Option<Process> {
        Process::open(self.record()?.process()).unwrap()
    }

    /// Ends the VM that the sandbox's store records, whatever its runtime identity: reattach ends
    /// an interrupted launch, and holds a VM that runs, which the stop then terminates through its
    /// verified identity. In a template store, the store's recovery ends the captures that a
    /// failed test interrupted, whose VMs run in its slots. Fails with the reason why a VM of the
    /// sandbox might still run.
    fn end_vm(&self) -> Result<(), String> {
        if !self.store.exists() {
            return Ok(());
        }
        let failed = |error: nvx::Error| describe(&error);
        if self.store.join(CAPTURES_NAME).is_dir() {
            let store = Store::open(&self.store).map_err(failed)?;
            let deadline = Instant::now() + Duration::from_secs(30);
            return store.recover_captures(deadline).map(drop).map_err(failed);
        }
        let store = SandboxStore::open(&self.store).map_err(failed)?;
        let recorded = (
            store.runtime().map_err(failed)?,
            store.journal().map_err(failed)?,
        );
        let identity = match recorded {
            (Some(record), _) => record.runtime().clone(),
            (None, Some(journal)) => journal.runtime().clone(),
            (None, None) => return Ok(()),
        };
        let mut locked = store
            .lock(Some(Instant::now() + Duration::from_secs(10)))
            .map_err(failed)?;
        let vmm = match supervisor::reattach(&mut locked, &identity).map_err(failed)? {
            Reattached::Live(vmm) | Reattached::Mismatched { vmm, .. } => vmm,
            Reattached::Exited { .. } => return Ok(()),
            Reattached::Indeterminate(reason) => return Err(reason),
        };
        let forced = Some(Duration::from_secs(10));
        let stopped = vmm
            .stop(&mut locked, Instant::now(), forced, |_, _| {
                Ok::<(), Infallible>(())
            })
            .map_err(failed)?;
        match stopped.outcome {
            StopOutcome::Graceful | StopOutcome::Forced => Ok(()),
            StopOutcome::TimedOut => Err(format!(
                "VMM process {} did not exit after it was terminated",
                vmm.record().process().pid
            )),
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        match self.end_vm() {
            Ok(()) => {
                let _ = fs::remove_dir_all(&self.root);
            }
            Err(reason) => eprintln!(
                "kept {} because a VM of its sandbox might still run: {reason}",
                self.root.display()
            ),
        }
    }
}

/// Describes `error`, with its causes.
pub(crate) fn describe(error: &dyn Error) -> String {
    let mut description = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        description = format!("{description}: {cause}");
        source = cause.source();
    }
    description
}

/// Returns the options of a launch or a creation that name the guest agent, which
/// `RUNTIME_ADAPTER_GUEST` names, and the hypervisor that `NVX_HYPERVISOR` selects.
fn guest_options() -> [OsString; 4] {
    let hypervisor = Hypervisor::from_env_or_default().expect("NVX_HYPERVISOR is valid");
    let guest_agent = env::var_os(GUEST_AGENT).unwrap_or_else(|| {
        panic!("{GUEST_AGENT} must name a static x86_64-unknown-linux-musl runtime-adapter-guest")
    });
    [
        OsString::from("--guest-agent"),
        guest_agent,
        OsString::from("--hypervisor"),
        OsString::from(hypervisor.as_str()),
    ]
}

/// Returns the epoch that a command printed.
pub(crate) fn epoch(printed: &Printed) -> u64 {
    printed["epoch"].parse().unwrap()
}
