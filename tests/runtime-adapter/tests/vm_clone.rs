//! The host program starts clones of templates on a real hypervisor, through public APIs only: VMs
//! that a restore of a generation of a template store starts, in place, which the store leases to
//! a sandbox, and which the guest agent's clone mode activates, as `runtime-adapter-host start`
//! asks.
//!
//! Each scenario captures a [`Template`] of its own, with 256 MiB of RAM, of a capacity of
//! 1 GiB, in a template store of its own, as `vm_template` does, of the platform's copy of the
//! fixtures' Ubuntu EROFS image, and starts its clones, each in a sandbox store of its own, which
//! stages the clone's images, as a launch's does: that copy, and for a clone over a scratch image,
//! a copy of the scratch template, which, as NVX requires of a clone's images, no one else may
//! change either.
//!
//! - One generation serves clones over a scratch image and in RAM, at once and in turn, at each
//!   memory target from the template's RAM to its capacity, which the guest has online. The store
//!   leases the generation to each, a stop releases it, and the clones change nothing of the
//!   generation's snapshot, and leave nothing in its payload.
//! - The store and NVX's plan refuse a clone that the template cannot serve before any VMM starts,
//!   each a failure of the request, as NVX attributes it by its evidence, with no VM: an image or a
//!   scratch image of another length than the template's, an image of a geometry that OpenVMM
//!   refuses, a memory target beyond its capacity or of a part of a block. The start then releases
//!   the lease, as it does once the adapter refuses images that the sandbox does not stage. A
//!   generation whose key is gone fails the plan too, as the template's failure, and the start
//!   quarantines it through its tombstone.
//! - A clone whose activation fails ends before it is ready, with the status of the failure's
//!   class, which attributes the failure, and the VMM's exit: a scratch image that holds no ext4
//!   file system, a journal that the clone's record forbids, a workload identity whose account the
//!   image lacks, and a host that sends anything but the clone's Activate first, a ping, a start, or
//!   an execution. The sandbox records the failure until the release of its lease.
//! - A clone runs its executions as its record's workload identity, once it is ready: the repair
//!   acknowledged the restore before the clone took its reset, the host's first message came
//!   after that, and the clone's Ready, its start, and its first execution came in that order.
//! - Each command after the start reattaches to the clone from a new process: the clone answers
//!   the Activate of its record again, in a new session of the same broker, with its Ready, without
//!   activating again, and serves the command.
//! - A host program of another runtime ends the clone through its verified identity, without a
//!   word to its guest, and releases its lease: the template still serves its other clones.
//! - A clone whose VM ends without a word of its guest once a stop found it running fails, as NVX
//!   attributes the loss of its status, before the stop, which stops nothing, releases its lease:
//!   the template still serves clones.
//! - A tombstone quarantines the generation of a running clone: the generation takes no new lease,
//!   while the clone keeps running, reattaches, stops, and releases its lease, and the generation's
//!   snapshot stays as the capture left it.
//! - A start that ended once the store leased the generation, before it recorded the clone,
//!   leaves a lease whose template store no record names, or only the record of an earlier clone,
//!   of another template: a stop keeps it, and only its own template store, which `--template`
//!   names, releases it, after which the sandbox leases a generation again. A sandbox that holds a
//!   lease must not move: a command on one that moved refuses it, rather than take for it the
//!   sandbox at the lease's path, which holds another lease, or none.
//! - The store checks the generation's eligibility at the host's time: on the boot that captured
//!   the template, by the host's monotonic clock, whatever UTC says, and after a reboot by UTC,
//!   which clock fixtures stand for. It refuses a generation that is no longer, or not yet,
//!   eligible, without a lease.
//! - A clone reads no more of its disks on its restore than its root needs, which a read of each
//!   disk to its end shows the guest's statistics count.
//! - A clone's time settles after it is ready, with no violation, after the downtime that the
//!   host's monotonic clock measured.
//!
//! The tests boot VMs, so they are ignored. They read what the `common` module lists, and take
//! the template's image and the scratch template from its fixtures.
//!
//! ```text
//! cargo test --locked --features host --test vm_clone -- --ignored
//! ```
#![cfg(feature = "host")]

mod common;

use std::collections::BTreeMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io;
use std::mem;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use nvx::activation::{ReadyActivation, StoreGeneration};
use nvx::exec::{Key, Limits, Timers, Window};
use nvx::host::artifacts::Artifacts;
use nvx::host::platform::Transport;
use nvx::host::process::{BootId, Process};
use nvx::host::store::{CallerFiles, LOG_NAME, SandboxStore};
use nvx::host::template::{
    CAPTURE_RECORD_NAME, CloneControl, ClonePlan, CloneRecords, Evidence, GENERATIONS_NAME,
    Generation, HostTime, KEY_NAME, Lease, MAX_DOWNTIME_NS, MEMORY_BLOCK_SIZE, Outcome, Progress,
    RestoreDeadlines, SNAPSHOT_NAME, Stage, Store, Teardown,
};
use nvx::protocol::CAPABILITY_LEN;
use nvx::protocol::root::ImageFormat;
use nvx::protocol::status;
use nvx::uuid::Uuid;
use runtime_adapter::GUEST_BUILD;
use runtime_adapter::capture::DEFAULT_SCRATCH_LENGTH;
use runtime_adapter::clone::{HOST_TIME_VARIABLE, Request, host_time_text};
use runtime_adapter::exec::{HostMessage, Start};
use runtime_adapter::frame::Message;
use runtime_adapter::hook::{Action, STEP_HOOK, Step, StepHook};
use runtime_adapter::host::Client;
use runtime_adapter::runtime::Runtime;
use runtime_adapter::staged::Staged;

use common::{Fixtures, Printed, Sandbox};

/// The exit status of the host program's command that failed, after a diagnostic.
const FAILED: i32 = 1;
/// The exit status of a command whose clone failed, once nothing of the clone runs, and the
/// command released its lease.
const CLONE_FAILED: i32 = 8;
/// The guest agent's build of a host program of another runtime.
const ANOTHER_BUILD: &str = "another-build";
/// Bytes in a mebibyte.
const MIB: u64 = 1024 * 1024;
/// The RAM of the scenarios' templates, in MiB.
const TEMPLATE_MEMORY: u64 = 256;
/// The RAM capacity of the scenarios' templates, in MiB.
const TEMPLATE_CAPACITY: u64 = 1024;
/// The generation of each scenario's template store that its template is.
const GENERATION: StoreGeneration = StoreGeneration::new(NonZeroU64::MIN);
/// The workload identity of the clones that run as one: nobody's, in the Ubuntu image.
const NOBODY: &str = "65534:65534";
/// A workload identity whose account the Ubuntu image lacks.
const STRANGER: &str = "4242:4242";
/// Bytes in a sector of the guest's disks, which their statistics count.
const SECTOR: u64 = 512;
/// How long a clone's time may take to settle once the clone is ready.
const TIME_SETTLEMENT: Duration = Duration::from_secs(30);
/// Nanoseconds in an hour.
const HOUR_NS: u64 = 3_600_000_000_000;
/// The error number with which the guest refuses an execution that names an identity in a
/// sandbox that has its own: `EINVAL`.
const EINVAL: i32 = 22;

/// The template of a scenario: the generation that a template store of its own captured, in the
/// scenario's sandbox, of the platform's copy of the fixtures' Ubuntu EROFS image, with
/// [`TEMPLATE_MEMORY`] MiB of RAM, of a capacity of [`TEMPLATE_CAPACITY`] MiB.
struct Template {
    /// The scenario's sandbox, whose store is the template store, until the template drops.
    sandbox: Option<Sandbox>,
    /// The platform's copy of the image, the template's placeholder, which the clones stage as
    /// their image too.
    image: PathBuf,
    /// The image's identity.
    identity: String,
    /// The fixtures.
    fixtures: Fixtures,
}

impl Template {
    /// Creates the sandbox of the scenario `name`, stages the platform's copy of the image, and
    /// captures the template in the sandbox's store.
    ///
    /// On Linux, a clone's control socket, `generations/1/LAUNCH.sock` in the template store,
    /// after the 32 hexadecimal digits of the launch, must fit the path of a Unix socket, so the
    /// scenarios' names are short.
    fn create(name: &str) -> Self {
        let mut sandbox = Sandbox::new(name);
        let fixtures = Fixtures::find();
        let image = stage_platform(&sandbox.root.join("platform"), &fixtures);
        let distro = erofs(&image);
        let (memory, capacity) = (TEMPLATE_MEMORY.to_string(), TEMPLATE_CAPACITY.to_string());
        let options = [
            OsStr::new("--distro"),
            &distro,
            OsStr::new("--distro-identity"),
            OsStr::new(&fixtures.identity),
            OsStr::new("--memory"),
            OsStr::new(&memory),
            OsStr::new("--memory-capacity"),
            OsStr::new(&capacity),
        ];
        let created = sandbox.create(&options, 0);
        assert_eq!(created["created"], "captured", "{created:?}");
        assert_eq!(created["generation"], GENERATION.to_string(), "{created:?}");
        Self {
            identity: fixtures.identity.clone(),
            sandbox: Some(sandbox),
            image,
            fixtures,
        }
    }

    /// Returns the template store's directory.
    fn dir(&self) -> &Path {
        let sandbox = self.sandbox.as_ref();
        &sandbox
            .expect("the sandbox lasts until the template drops")
            .store
    }

    /// Returns the template store.
    fn store(&self) -> Store {
        Store::open(self.dir()).unwrap()
    }

    /// Returns the template's generation, as the store's index lists it.
    fn generation(&self) -> Generation {
        let index = self.store().index().unwrap();
        index.get(GENERATION).cloned().expect("the store holds it")
    }

    /// Returns the leases of the template's generation.
    fn leases(&self) -> Vec<Lease> {
        self.store().leases(GENERATION).unwrap()
    }

    /// Returns the payload of the template's generation.
    fn payload(&self) -> PathBuf {
        self.dir()
            .join(GENERATIONS_NAME)
            .join(GENERATION.to_string())
    }

    /// Returns a copy of the platform's image of `length` bytes, beside it, without a manifest,
    /// at `name`.
    fn variant(&self, name: &str, length: u64) -> PathBuf {
        let copy = self.image.with_file_name(name);
        fs::copy(&self.image, &copy).unwrap();
        File::options()
            .write(true)
            .open(&copy)
            .unwrap()
            .set_len(length)
            .unwrap();
        copy
    }

    /// Stages the template's image in the store of `sandbox`, which it creates, for a clone in
    /// RAM.
    fn stage(&self, sandbox: &mut Sandbox) {
        let distro = erofs(&self.image);
        let options = [
            OsStr::new("--distro"),
            &distro,
            OsStr::new("--distro-identity"),
            OsStr::new(&self.identity),
        ];
        sandbox.run("stage", &options, 0);
    }

    /// Starts a clone in a new sandbox of the scenario `name`, as `start` asks, checks that the
    /// start exits with `status`, and returns the clone.
    fn start(&self, name: &str, start: StartOf<'_>, status: i32) -> Cloned {
        let mut sandbox = Sandbox::new(name);
        let distro = erofs(start.image.unwrap_or(&self.image));
        // NVX restores a clone only over images that no one else may change, so the scratch image
        // lies in a directory of the sandbox's own, as the platform's image lies in the platform's.
        let images = sandbox.root.join("images");
        SandboxStore::create(&images).unwrap();
        let scratch = images.join("scratch.ext4");
        match start.upper {
            Upper::Ram => {}
            Upper::Scratch => {
                private_copy(&self.fixtures.scratch, &scratch);
            }
            Upper::Resized(length) => {
                let file = private_copy(&self.fixtures.scratch, &scratch);
                file.set_len(length).unwrap();
            }
            Upper::Zeros => private_file(&scratch)
                .set_len(DEFAULT_SCRATCH_LENGTH)
                .unwrap(),
        }
        let upper = match start.upper {
            Upper::Ram => OsString::from("ram"),
            _ => prefixed("scratch:", &scratch),
        };
        let identity = OsStr::new(&self.identity);
        if !start.unstaged {
            let mut staging = vec![
                OsStr::new("--distro"),
                &distro,
                OsStr::new("--distro-identity"),
                identity,
            ];
            if start.upper != Upper::Ram {
                staging.extend([OsStr::new("--upper"), &upper]);
            }
            sandbox.run("stage", &staging, 0);
        }
        let generation = GENERATION.to_string();
        let mut options = vec![
            OsStr::new("--template"),
            self.dir().as_os_str(),
            OsStr::new("--generation"),
            OsStr::new(&generation),
            OsStr::new("--distro"),
            &distro,
            OsStr::new("--distro-identity"),
            identity,
            OsStr::new("--upper"),
            &upper,
        ];
        options.extend(start.options.iter().map(OsStr::new));
        let time = start.time.map(|time| OsString::from(host_time_text(&time)));
        let environment: Vec<(&str, &OsStr)> = time
            .iter()
            .map(|time| (HOST_TIME_VARIABLE, time.as_os_str()))
            .collect();
        let started = sandbox.start("start", &options, &environment);
        let started = sandbox.finish(started, status);
        Cloned { sandbox, started }
    }
}

impl Drop for Template {
    /// Removes the template's sandbox, as a sandbox's drop does, unless a clone whose VM might
    /// still run leases the generation: one whose sandbox a test that failed kept, since it could
    /// not confirm that the clone's VM ended. Its VMM restores the generation's snapshot in place,
    /// and reads the platform's image, which the template's sandbox holds too, so the template
    /// stays with it. A clone's sandbox drops before the template, which the scenario creates
    /// first.
    fn drop(&mut self) {
        let Some(sandbox) = self.sandbox.take() else {
            return;
        };
        let kept = Store::open(&sandbox.store)
            .and_then(|store| store.leases(GENERATION))
            .map(|leases| {
                leases
                    .iter()
                    .map(Lease::sandbox)
                    .filter(|clone| clone.exists())
                    .map(|clone| clone.display().to_string())
                    .collect::<Vec<_>>()
            });
        let reason = match kept {
            Ok(kept) if kept.is_empty() => {
                drop(sandbox);
                return;
            }
            Ok(kept) => format!("the clones in {} might still run", kept.join(", ")),
            Err(error) => format!("its leases cannot be read: {}", common::describe(&error)),
        };
        eprintln!("kept {} because {reason}", sandbox.root.display());
        mem::forget(sandbox);
    }
}

/// The upper of a clone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Upper {
    /// RAM.
    #[default]
    Ram,
    /// A copy of the scratch template, an empty ext4 file system of the template's scratch
    /// length.
    Scratch,
    /// A copy of the scratch template of this many bytes.
    Resized(u64),
    /// Zeros of the template's scratch length, which hold no file system.
    Zeros,
}

/// What a scenario asks of the start of a clone.
#[derive(Default)]
struct StartOf<'a> {
    /// The image, or the template's image by default.
    image: Option<&'a Path>,
    /// The upper.
    upper: Upper,
    /// The start's other options.
    options: &'a [&'a str],
    /// The host's time at which the store checks the generation's eligibility, as the clock
    /// fixture, or the time that the host program samples by default.
    time: Option<HostTime>,
    /// Whether the sandbox's store stages no image.
    unstaged: bool,
}

/// A clone of a scenario's template, in a sandbox of its own, as its start left it.
struct Cloned {
    /// The clone's sandbox.
    sandbox: Sandbox,
    /// What the start printed.
    started: Printed,
}

impl Cloned {
    /// Checks that the start started the clone, as the [module documentation](self) describes,
    /// leasing the generation, and recorded its VM, which runs.
    fn assert_started(&self) {
        let started = &self.started;
        assert_eq!(started["generation"], GENERATION.to_string(), "{started:?}");
        let lease = &started["lease"];
        assert!(
            lease.len() == 32 && lease.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "{started:?}"
        );
        started["downtime"].parse::<u64>().unwrap();
        let facts = ["root", "build", "table", "mapped", "sandbox"].map(|key| &started[key]);
        assert_eq!(
            facts,
            ["ready", GUEST_BUILD, "applied", "0", "started"],
            "{started:?}"
        );
        let record = self.sandbox.record().expect("the start recorded its VM");
        assert_eq!(started["pid"], record.process().pid.to_string());
        assert!(!started.contains_key("released"), "{started:?}");
    }

    /// Runs `probes` in the clone's root, and returns what they found, each fact as
    /// `PROBE.KEY`.
    fn probe(&mut self, probes: &[&str]) -> Printed {
        let probes = probes.iter().map(OsStr::new).collect::<Vec<_>>();
        let printed = self.sandbox.run("probe", &probes, 0);
        assert_eq!(printed["reattached"], "live");
        assert_eq!(printed["pid"], self.started["pid"]);
        printed
    }

    /// Runs one execution of `arguments`, its options and its program, in the clone's root, and
    /// returns what it did, each fact as `exec.1.KEY`.
    fn exec(&mut self, arguments: &[&str]) -> Printed {
        let arguments = arguments.iter().map(OsStr::new).collect::<Vec<_>>();
        let printed = self.sandbox.run("exec", &arguments, 0);
        assert_eq!(printed["reattached"], "live");
        printed
    }

    /// Stops the clone, and checks that it stopped gracefully, and that the stop released the
    /// clone's lease.
    fn stop(&mut self) {
        let stopped = self.sandbox.run("stop", &[], 0);
        let facts = ["stopped", "outcome", "teardown"].map(|key| &stopped[key]);
        assert_eq!(facts, ["graceful", "success 0", "complete"], "{stopped:?}");
        assert_eq!(stopped["released"], self.started["lease"], "{stopped:?}");
        assert!(self.sandbox.record().is_none());
        let store = SandboxStore::open(&self.sandbox.store).unwrap();
        assert!(Lease::of(&store).unwrap().is_none());
    }

    /// Checks that the start refused the clone before any VMM started, and released the lease
    /// that it took, if it took one, with a diagnostic that says `problem`.
    fn assert_refused(&self, problem: &str) {
        let started = &self.started;
        assert!(!started.contains_key("pid"), "{started:?}");
        assert_eq!(started.get("released"), started.get("lease"), "{started:?}");
        let stderr = self.sandbox.stderr();
        assert!(stderr.contains(problem), "{problem:?} in {stderr}");
        self.sandbox.assert_unlaunched();
        let store = SandboxStore::open(&self.sandbox.store).unwrap();
        assert!(Lease::of(&store).unwrap().is_none());
    }
}

/// Stages the platform's copy of the fixtures' Ubuntu EROFS image in the directory `platform`,
/// beside a copy of its manifest, and returns it.
///
/// A platform stages its trusted artifacts where no one else may change them, and NVX refuses an
/// image that anyone else may change. So the directory is private to the current user, as NVX
/// makes a sandbox's store, whose access control list on Windows passes on to the files created in
/// it.
fn stage_platform(platform: &Path, fixtures: &Fixtures) -> PathBuf {
    SandboxStore::create(platform).unwrap();
    let [image, _] = [&fixtures.distro, &fixtures.manifest].map(|source| {
        let copy = platform.join(source.file_name().unwrap());
        private_copy(source, &copy);
        copy
    });
    image
}

/// Creates the file `path`, which no one else may change, in a directory private to the current
/// user, and returns it, open for writing: of the mode 0644 on Unix, whatever the umask, and on
/// Windows of the directory's access control list.
fn private_file(path: &Path) -> File {
    let file = File::create(path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o644))
            .unwrap();
    }
    file
}

/// Copies the file `source` to `copy`, which [`private_file`] creates, and returns the copy,
/// open for writing.
fn private_copy(source: &Path, copy: &Path) -> File {
    let mut file = private_file(copy);
    io::copy(&mut File::open(source).unwrap(), &mut file).unwrap();
    file
}

/// Returns `path` after `prefix`, as an option's value names a file.
fn prefixed(prefix: &str, path: &Path) -> OsString {
    let mut value = OsString::from(prefix);
    value.push(path);
    value
}

/// Returns the value of `--distro` for the EROFS image at `path`.
fn erofs(path: &Path) -> OsString {
    prefixed("erofs:", path)
}

/// Returns the names of the entries of the directory `path`, sorted.
fn names(path: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// Returns each file below the directory `snapshot`, by its path in it, with its length and its
/// modification time.
fn snapshot_files(snapshot: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
    let mut files = Vec::new();
    let mut directories = vec![snapshot.to_owned()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory).unwrap() {
            let entry = entry.unwrap();
            let metadata = entry.metadata().unwrap();
            if metadata.is_dir() {
                directories.push(entry.path());
                continue;
            }
            let path = entry.path().strip_prefix(snapshot).unwrap().to_owned();
            files.push((path, metadata.len(), metadata.modified().unwrap()));
        }
    }
    files.sort();
    files
}

/// Checks that the payload of the template's generation holds what a capture published alone,
/// and, while clones run, their control sockets on Linux: no copy of a scratch image, say.
fn assert_payload(template: &Template, running: bool) {
    let published = [CAPTURE_RECORD_NAME, KEY_NAME, SNAPSHOT_NAME];
    let names = names(&template.payload());
    let (template_files, others): (Vec<_>, Vec<_>) = names
        .iter()
        .partition(|name| published.contains(&name.as_str()));
    assert_eq!(template_files, published, "{names:?}");
    let sockets = running && cfg!(target_os = "linux");
    assert!(
        others
            .iter()
            .all(|name| sockets && name.len() == 37 && name.ends_with(".sock")),
        "{names:?}"
    );
}

/// Checks that a clone's `memory` probe found the memory target `target`, in MiB, online.
fn assert_memory(facts: &Printed, target: u64) {
    let number = |key: &str| facts[&format!("memory.{key}")].parse::<u64>().unwrap();
    let block = number("block-size");
    assert!(
        block > 0 && MEMORY_BLOCK_SIZE.is_multiple_of(block),
        "{facts:?}"
    );
    assert_eq!(number("online"), target * MIB, "{facts:?}");
    assert_eq!(number("blocks") * block, target * MIB, "{facts:?}");
    assert!(number("total") <= target * MIB, "{facts:?}");
}

/// Checks the root of a clone over `upper`, as its `mounts` and `contents` probes found it, in
/// the overlay mode that trusts the lowers, or not.
fn assert_root(facts: &Printed, upper: Upper, trusted: bool) {
    assert_eq!(facts["mounts.distro.count"], "1", "{facts:?}");
    assert_eq!(facts["mounts.distro.type"], "erofs", "{facts:?}");
    let (used, unused, kind) = match upper {
        Upper::Ram => ("tmpfs", "scratch", "tmpfs"),
        _ => ("scratch", "tmpfs", "ext4"),
    };
    assert_eq!(facts[&format!("mounts.{used}.count")], "1", "{facts:?}");
    assert_eq!(facts[&format!("mounts.{used}.type")], kind, "{facts:?}");
    assert_eq!(facts[&format!("mounts.{unused}.count")], "0", "{facts:?}");
    assert_eq!(facts["mounts.root.type"], "overlay", "{facts:?}");
    let overlay = facts["mounts.root.super"].split(',').collect::<Vec<_>>();
    for option in ["metacopy=on", "redirect_dir=on", "xino=on"] {
        assert_eq!(
            overlay.contains(&option),
            trusted,
            "{option} in {overlay:?}"
        );
    }
    let contents = ["os-release.id", "write.root", "write.upper", "lower.write"]
        .map(|key| &facts[&format!("contents.{key}")]);
    assert_eq!(
        contents,
        ["ubuntu", "runtime-adapter", "runtime-adapter", "refused"],
        "{facts:?}"
    );
}

/// Checks the events of a clone's activation that its `activation` probe found: the repair
/// acknowledged the restore, and the clone took its reset, the host's first message, its Ready,
/// and its start, in that order, after the acknowledgement. Returns when each came, in
/// microseconds after the acknowledgement.
fn assert_timeline(facts: &Printed) -> [u64; 4] {
    assert_eq!(facts["activation.acknowledged"], "yes", "{facts:?}");
    let events = ["reset", "message", "ready", "started"].map(|event| {
        let value = &facts[&format!("activation.{event}")];
        value
            .parse::<u64>()
            .unwrap_or_else(|_| panic!("the clone's {event} is {value}: {facts:?}"))
    });
    assert!(events.is_sorted(), "{facts:?}");
    events
}

/// Returns the record of a clone's failure that a command printed, as `failure.KEY` lines: its
/// outcome, its evidence, the stage at which the clone failed, how far the clone got, the guest's
/// status, and how the teardown ended, each `-` if the command did not print it.
fn failure(printed: &Printed) -> [&str; 6] {
    [
        "outcome", "evidence", "stage", "progress", "status", "teardown",
    ]
    .map(|key| {
        printed
            .get(&format!("failure.{key}"))
            .map_or("-", String::as_str)
    })
}

/// Returns the disks of a probe's `disks` fact, `NAME:NUMBER` or `NAME:NUMBER:ACCESS` each, with
/// their numbers.
fn disks(fact: &str) -> BTreeMap<String, u64> {
    fact.split(' ')
        .map(|disk| {
            let mut parts = disk.split(':');
            let name = parts.next().unwrap().to_owned();
            let number = parts.next().and_then(|number| number.parse().ok());
            (
                name,
                number.unwrap_or_else(|| panic!("{disk:?} in {fact:?}")),
            )
        })
        .collect()
}

/// Returns a boot of this host other than `boot`.
fn other_boot(boot: BootId) -> BootId {
    let text = boot.to_string();
    let first = if text.starts_with('0') { "1" } else { "0" };
    format!("{first}{}", &text[1..]).parse().unwrap()
}

/// The adapter's framing of a clone's control session that sends `early`, which the host must not
/// send before the clone is ready, in place of the clone's Activate, and, once the clone ended its
/// session, waits for the clone's VMM to exit, so that the restore sequence, which then ends the
/// launch, ends it once the VM ended on its own.
struct Early {
    /// What the host sends first.
    early: Message,
    /// The clone's VMM.
    vmm: Process,
    /// Whether the clone's VMM exited once the clone ended the session.
    exited: bool,
}

impl CloneControl for Early {
    type Session = Client;
    type Error = io::Error;

    fn attach(
        &mut self,
        transport: Box<dyn Transport>,
        capability: &[u8; CAPABILITY_LEN],
        deadline: Instant,
    ) -> io::Result<Client> {
        Client::attach(transport, capability, deadline).map_err(io::Error::other)
    }

    fn activate(
        &mut self,
        session: &mut Client,
        _record: &[u8],
        deadline: Instant,
    ) -> io::Result<Option<ReadyActivation>> {
        session
            .send(&self.early, Some(deadline))
            .map_err(io::Error::other)?;
        let answer = session.receive(Some(deadline));
        self.exited = self.vmm.wait(deadline)?;
        Err(match answer {
            Ok(message) => io::Error::other(format!("the clone answered {message:?}")),
            Err(error) => io::Error::other(error),
        })
    }

    fn start(&mut self, _session: &mut Client, _deadline: Instant) -> io::Result<()> {
        Err(io::Error::other("the clone is never ready"))
    }

    fn lost(&self, _error: &io::Error) -> bool {
        false
    }
}

/// Starts a clone of `template` in RAM, in a sandbox of its own, through NVX's public API, whose
/// host sends `early` before the clone's Activate, and checks that the clone ended before it was
/// ready, with the status of the host's session, which attributes the failure to no one, as the
/// sandbox records it, and that the VMM exited, then releases the clone's lease, which removes the
/// record.
fn refuses_early(template: &Template, early: Message) {
    let mut sandbox = Sandbox::new("cl-e");
    template.stage(&mut sandbox);
    let store = template.store();
    let artifacts = env::var_os(Artifacts::ENV_DIR).unwrap();
    let runtime = Runtime::from_dir(artifacts, GUEST_BUILD).unwrap();
    let request = Request {
        distro: template.image.clone(),
        format: ImageFormat::Erofs,
        identity: Uuid::parse(&template.identity).unwrap(),
        scratch: None,
        memory: None,
        workload_identity: None,
        overlay_mode: None,
        scratch_journal: None,
    };
    let plan = request.plan().unwrap();
    let staged = Staged::read(&sandbox.store).unwrap().unwrap();
    let key = store.read_key(&template.generation()).unwrap();
    let clone = request
        .clone_request(plan.clone(), &staged.bindings(&plan), key.memory())
        .unwrap();
    let now = HostTime::now().unwrap();
    let mut acquired = store
        .acquire(GENERATION, &sandbox.store, &CallerFiles::none(), &now, None)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let planned = ClonePlan::new(&store, &mut acquired, &clone, deadline).unwrap();
    let restoring = planned
        .launch(&mut acquired, &runtime.artifacts.openvmm, &runtime.identity)
        .unwrap();
    let vmm = Process::open(restoring.starting().process().reference())
        .unwrap()
        .expect("the clone's VMM runs");
    let mut control = Early {
        early: early.clone(),
        vmm,
        exited: false,
    };
    let failure = restoring
        .activate(&mut control, RestoreDeadlines::default())
        .unwrap_err();
    let log = || fs::read_to_string(sandbox.store.join(LOG_NAME)).unwrap_or_default();
    let problem = common::describe(&failure);
    assert!(
        !problem.contains("the clone answered"),
        "{early:?}: {problem}"
    );
    assert!(control.exited, "{early:?}: {problem}\n{}", log());
    let record = failure.record();
    let ended = (
        record.outcome,
        record.evidence,
        record.stage,
        record.progress,
        record.status,
        &record.teardown,
    );
    let expected = (
        Outcome::Unknown,
        Evidence::Protocol,
        Stage::Activation,
        Progress::Activating,
        Some(status::ACTIVATION_PROTOCOL),
        &Teardown::Exited,
    );
    assert_eq!(ended, expected, "{early:?}: {problem}\n{}", log());
    assert_eq!(record.lease, acquired.lease.id());
    // The sandbox records the failure, which keeps the clone from launching again, until the
    // release of its lease.
    let records = CloneRecords::read(&acquired.locked).unwrap().unwrap();
    assert_eq!(records.failure.as_ref(), Some(record));
    assert!(records.journal.is_none() && records.runtime.is_none());
    let released = store.release(&mut acquired.locked).unwrap();
    assert_eq!(released.map(|lease| lease.id()), Some(acquired.lease.id()));
    assert!(CloneRecords::read(&acquired.locked).unwrap().is_none());
    assert!(template.leases().is_empty());
}

/// Returns the limits of the executions of the scenarios that the host starts itself.
fn limits() -> Limits {
    Limits::new(16 * 1024, Window::frames(4), Window::frames(8))
        .unwrap()
        .shared_output()
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn one_generation_serves_clones_of_each_upper_and_memory_target() {
    let template = Template::create("cl-reuse");
    let snapshot = template.payload().join(SNAPSHOT_NAME);
    let captured = snapshot_files(&snapshot);

    // Two clones at once: one over a scratch image, at the template's RAM, which trusts its
    // lowers, as the guest does by default, and one in RAM, at twice the template's RAM, which
    // does not.
    let scratch = StartOf {
        upper: Upper::Scratch,
        ..StartOf::default()
    };
    let mut over_scratch = template.start("cl-a", scratch, 0);
    let options = ["--memory", "512", "--overlay-mode", "untrusted-lowers"];
    let in_ram = StartOf {
        options: &options,
        ..StartOf::default()
    };
    let mut in_ram = template.start("cl-b", in_ram, 0);
    for cloned in [&over_scratch, &in_ram] {
        cloned.assert_started();
        // On the boot that captured the template, the host's monotonic clock measures the
        // downtime.
        assert_eq!(
            cloned.started["source"], "monotonic",
            "{:?}",
            cloned.started
        );
    }
    assert_ne!(over_scratch.started["lease"], in_ram.started["lease"]);

    // The store leases the generation to both, by their sandboxes' paths.
    let mut leases = template
        .leases()
        .iter()
        .map(|lease| (lease.id().to_string(), lease.sandbox().to_owned()))
        .collect::<Vec<_>>();
    leases.sort();
    let mut expected = [&over_scratch, &in_ram].map(|cloned| {
        let root = cloned.sandbox.root.file_name().unwrap();
        (
            cloned.started["lease"].clone(),
            Path::new(root).join("store"),
        )
    });
    expected.sort();
    assert_eq!(leases.len(), expected.len(), "{leases:?}");
    for ((lease, sandbox), (expected, store)) in leases.iter().zip(&expected) {
        assert_eq!(lease, expected);
        assert!(sandbox.ends_with(store), "{}", sandbox.display());
    }
    assert_payload(&template, true);

    let probes = ["memory", "mounts", "contents", "activation"];
    let facts = over_scratch.probe(&probes);
    assert_memory(&facts, TEMPLATE_MEMORY);
    assert_root(&facts, Upper::Scratch, true);
    assert_timeline(&facts);
    let facts = in_ram.probe(&probes);
    assert_memory(&facts, 512);
    assert_root(&facts, Upper::Ram, false);
    assert_timeline(&facts);
    over_scratch.stop();
    assert_eq!(template.leases().len(), 1);
    in_ram.stop();
    assert!(template.leases().is_empty());

    // Then another over a scratch image, at the template's RAM capacity.
    let options = ["--memory", "1024"];
    let at_capacity = StartOf {
        upper: Upper::Scratch,
        options: &options,
        ..StartOf::default()
    };
    let mut at_capacity = template.start("cl-c", at_capacity, 0);
    at_capacity.assert_started();
    let facts = at_capacity.probe(&["memory", "mounts", "contents"]);
    assert_memory(&facts, TEMPLATE_CAPACITY);
    assert_root(&facts, Upper::Scratch, true);
    at_capacity.stop();
    assert!(template.leases().is_empty());

    // The clones changed nothing of the generation's snapshot, which they restored in place, and
    // left nothing in its payload.
    assert_eq!(snapshot_files(&snapshot), captured);
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_start_refuses_what_the_template_cannot_serve_before_any_vmm_starts() {
    let template = Template::create("cl-refuse");
    let length = fs::metadata(&template.image).unwrap().len();
    let longer = template.variant("longer.erofs", length + 4096);
    let shorter = template.variant("shorter.erofs", length - 4096);
    // OpenVMM attaches block devices of whole 512-byte sectors alone.
    let ragged = template.variant("ragged.erofs", length + 100);
    let placeholder = |found: u64| {
        format!("is {found} bytes long, not the {length} bytes of the template's placeholder")
    };
    let resized = 2 * DEFAULT_SCRATCH_LENGTH;
    let beyond = ["--memory", "2048"];
    let part = ["--memory", "320"];
    let cases = [
        (
            StartOf {
                image: Some(&longer),
                ..StartOf::default()
            },
            "supplied-length",
            placeholder(length + 4096),
        ),
        (
            StartOf {
                image: Some(&shorter),
                upper: Upper::Scratch,
                ..StartOf::default()
            },
            "supplied-length",
            placeholder(length - 4096),
        ),
        (
            StartOf {
                image: Some(&ragged),
                ..StartOf::default()
            },
            "supplied-geometry",
            format!("of {} bytes cannot be attached", length + 100),
        ),
        (
            StartOf {
                upper: Upper::Resized(resized),
                ..StartOf::default()
            },
            "supplied-length",
            format!(
                "is {resized} bytes long, not the {DEFAULT_SCRATCH_LENGTH} bytes of the \
                 template's scratch"
            ),
        ),
        (
            StartOf {
                options: &beyond,
                ..StartOf::default()
            },
            "invalid-targets",
            format!("cannot restore with {} bytes of RAM", 2048 * MIB),
        ),
        (
            StartOf {
                options: &part,
                ..StartOf::default()
            },
            "invalid-targets",
            format!("cannot restore with {} bytes of RAM", 320 * MIB),
        ),
    ];
    for (start, evidence, problem) in cases {
        let refused = template.start("cl-r", start, CLONE_FAILED);
        // NVX's plan refused the request before anything of the clone's launch, so the start
        // released the lease that the store granted.
        assert!(refused.started.contains_key("lease"), "{problem}");
        assert_eq!(
            failure(&refused.started),
            ["request", evidence, "plan", "planned", "none", "no-vm"],
            "{problem}: {:?}",
            refused.started
        );
        assert_eq!(refused.started["quarantined"], "no");
        refused.assert_refused(&problem);
        assert!(template.leases().is_empty());
    }

    // The adapter refuses images that the sandbox does not stage, and releases the lease.
    let start = StartOf {
        unstaged: true,
        ..StartOf::default()
    };
    let refused = template.start("cl-r", start, FAILED);
    assert!(refused.started.contains_key("lease"));
    assert!(!refused.started.contains_key("failure.outcome"));
    refused.assert_refused("; stage the images first");
    assert!(template.leases().is_empty());

    // The store holds no other generation.
    let mut sandbox = Sandbox::new("cl-r");
    template.stage(&mut sandbox);
    let options = [
        OsStr::new("--template"),
        template.dir().as_os_str(),
        OsStr::new("--generation"),
        OsStr::new("2"),
        OsStr::new("--distro"),
        &erofs(&template.image),
        OsStr::new("--distro-identity"),
        OsStr::new(&template.identity),
        OsStr::new("--upper"),
        OsStr::new("ram"),
    ];
    let printed = sandbox.run("start", &options, FAILED);
    assert!(printed.is_empty(), "{printed:?}");
    assert!(sandbox.stderr().contains("holds no generation 2"));
    sandbox.assert_unlaunched();

    // A generation whose key is gone is malformed: NVX's plan, which reads the key once the store
    // leased the generation, attributes the failure to the template, and the start quarantines
    // the generation through its tombstone, which refuses a later start without a lease.
    fs::remove_file(template.payload().join(KEY_NAME)).unwrap();
    let refused = template.start("cl-r", StartOf::default(), CLONE_FAILED);
    assert!(
        refused.started.contains_key("lease"),
        "{:?}",
        refused.started
    );
    assert_eq!(
        failure(&refused.started),
        [
            "template",
            "malformed-generation",
            "plan",
            "planned",
            "none",
            "no-vm"
        ],
        "{:?}",
        refused.started
    );
    assert_eq!(refused.started["quarantined"], "yes");
    refused.assert_refused(" is missing");
    assert!(template.leases().is_empty());
    assert_eq!(template.store().tombstones().unwrap(), [GENERATION]);
    let quarantined = template.start("cl-r", StartOf::default(), FAILED);
    assert!(quarantined.started.is_empty(), "{:?}", quarantined.started);
    quarantined.assert_refused(&format!(
        "a tombstone quarantines the generation {GENERATION} of the template store"
    ));
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_clone_whose_activation_fails_ends_before_it_is_ready() {
    let template = Template::create("cl-unready");
    let forbidden = ["--scratch-journal", "forbidden"];
    let stranger = ["--workload-identity", STRANGER];
    let cases = [
        (
            StartOf {
                upper: Upper::Zeros,
                ..StartOf::default()
            },
            ("unusable-root", status::ACTIVATION_ROOT),
        ),
        (
            StartOf {
                upper: Upper::Scratch,
                options: &forbidden,
                ..StartOf::default()
            },
            ("unusable-root", status::ACTIVATION_ROOT),
        ),
        (
            StartOf {
                options: &stranger,
                ..StartOf::default()
            },
            ("invalid-settings", status::ACTIVATION_WORKLOAD),
        ),
    ];
    for (start, (evidence, expected)) in cases {
        let ended = template.start("cl-u", start, CLONE_FAILED);
        let printed = &ended.started;
        // The VM started, and ended before the clone was ready, with the status of the failure's
        // class, which attributes the failure to the request, and the start released the lease.
        assert!(printed.contains_key("pid"), "{printed:?}");
        assert!(!printed.contains_key("root"), "{printed:?}");
        let status = expected.to_string();
        assert_eq!(
            failure(printed),
            [
                "request",
                evidence,
                "activation",
                "activating",
                status.as_str(),
                "exited"
            ],
            "{printed:?}"
        );
        assert_eq!(printed["quarantined"], "no");
        assert_eq!(printed["released"], printed["lease"], "{printed:?}");
        assert!(ended.sandbox.record().is_none());
        // The release removed the records of the clone's launch.
        let store = SandboxStore::open(&ended.sandbox.store).unwrap();
        assert!(CloneRecords::read(&store).unwrap().is_none());
        assert!(template.leases().is_empty());
    }

    // Until the clone is ready, it takes nothing but its Activate: a ping, a start, and the start
    // of an execution, before it, each end the clone with the status of the host's session.
    let timers = Timers::new(Duration::from_secs(1), Duration::from_secs(5));
    let execution = Start::new(Key::new(1, 1), limits(), timers, [b"/bin/true".to_vec()]);
    for early in [
        Message::Ping,
        Message::Start,
        HostMessage::Start(execution).encode(),
    ] {
        refuses_early(&template, early);
    }
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_clone_runs_its_executions_as_its_workload_identity_once_it_is_ready() {
    let template = Template::create("cl-ident");
    let options = ["--workload-identity", NOBODY];
    let start = StartOf {
        upper: Upper::Scratch,
        options: &options,
        ..StartOf::default()
    };
    let mut cloned = template.start("cl-i", start, 0);
    cloned.assert_started();
    let facts = cloned.probe(&["activation"]);
    let events = assert_timeline(&facts);
    assert_eq!(facts["activation.execution"], "none", "{facts:?}");

    // Every execution runs as the record's workload identity, and one that names its own is
    // refused before its program starts.
    let ran = cloned.exec(&["--", "/bin/sh", "-c", "id -u; id -g"]);
    let facts = ["launch", "status", "stdout", "stderr"].map(|key| &ran[&format!("exec.1.{key}")]);
    assert_eq!(
        facts,
        ["started", "exit 0", "65534\\n65534\\n", ""],
        "{ran:?}"
    );
    let refused = cloned.exec(&["--identity", "1000:1000", "--", "/bin/true"]);
    let launch = format!("failed 0 {EINVAL}");
    assert_eq!(refused["exec.1.launch"], launch, "{refused:?}");

    // The first execution came once the clone's sandbox started.
    let facts = cloned.probe(&["activation"]);
    let execution = facts["activation.execution"].parse::<u64>().unwrap();
    assert!(execution >= events[3], "{facts:?}");
    assert_eq!(assert_timeline(&facts), events);
    cloned.stop();
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_generation_is_eligible_by_the_clock_of_its_capture() {
    let template = Template::create("cl-clock");
    let contract = template.generation().time().clone();
    let now = HostTime::now().unwrap();
    // The scenario runs on the host's boot that captured the template.
    assert_eq!(
        (now.host_id, now.boot_id, now.clock),
        (contract.host_id, contract.boot_id, contract.host_clock)
    );
    let started = |time: Option<HostTime>| {
        let start = StartOf {
            time,
            ..StartOf::default()
        };
        let mut cloned = template.start("cl-k", start, 0);
        cloned.assert_started();
        cloned.stop();
        cloned.started
    };

    // On that boot, the host's monotonic clock measures the downtime, whatever UTC says.
    let printed = started(None);
    assert_eq!(printed["source"], "monotonic", "{printed:?}");
    let behind = HostTime {
        utc_ns: contract.capture_utc_ns - 24 * HOUR_NS,
        ..now
    };
    let printed = started(Some(behind));
    assert_eq!(printed["source"], "monotonic", "{printed:?}");
    let downtime = behind.monotonic_ns - contract.capture_monotonic_ns;
    assert_eq!(printed["downtime"], downtime.to_string());

    // After a reboot, UTC does.
    let rebooted = HostTime {
        boot_id: other_boot(contract.boot_id),
        monotonic_ns: 1,
        utc_ns: contract.capture_utc_ns + HOUR_NS,
        ..now
    };
    let printed = started(Some(rebooted));
    assert_eq!(printed["source"], "utc", "{printed:?}");
    assert_eq!(printed["downtime"], HOUR_NS.to_string());

    // A generation that is no longer eligible, or not yet, takes no lease.
    let cases = [
        (
            HostTime {
                monotonic_ns: contract.capture_monotonic_ns + MAX_DOWNTIME_NS + 1,
                ..now
            },
            "is no longer eligible: after a downtime of",
            "by the host's monotonic clock",
        ),
        (
            HostTime {
                utc_ns: contract.capture_utc_ns + MAX_DOWNTIME_NS + 1,
                ..rebooted
            },
            "is no longer eligible: after a downtime of",
            "by UTC",
        ),
        (
            HostTime {
                utc_ns: contract.capture_utc_ns - 1,
                ..rebooted
            },
            "is not eligible yet: by UTC",
            "1 ns before the anchor of the capture",
        ),
    ];
    for (time, state, measured) in cases {
        let start = StartOf {
            time: Some(time),
            ..StartOf::default()
        };
        let refused = template.start("cl-k", start, FAILED);
        assert!(refused.started.is_empty(), "{:?}", refused.started);
        let generation = format!("the generation {GENERATION} of the template store");
        refused.assert_refused(&generation);
        let stderr = refused.sandbox.stderr();
        assert!(
            stderr.contains(state) && stderr.contains(measured),
            "{stderr}"
        );
        assert!(template.leases().is_empty());
    }
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_clone_reads_no_more_of_its_disks_than_its_root_needs() {
    let template = Template::create("cl-reads");
    for upper in [Upper::Scratch, Upper::Ram] {
        let start = StartOf {
            upper,
            ..StartOf::default()
        };
        let mut cloned = template.start("cl-d", start, 0);
        cloned.assert_started();
        let facts = cloned.probe(&["devices", "reads"]);
        let sizes = disks(&facts["devices.disks"]);
        let restored = disks(&facts["reads.disks"]);
        assert_eq!(
            sizes.keys().collect::<Vec<_>>(),
            restored.keys().collect::<Vec<_>>(),
            "{facts:?}"
        );
        // The image and the scratch device, whose lengths are the template's.
        let mut lengths = sizes
            .values()
            .map(|sectors| sectors * SECTOR)
            .collect::<Vec<_>>();
        lengths.sort_unstable();
        let mut expected = [
            fs::metadata(&template.image).unwrap().len(),
            DEFAULT_SCRATCH_LENGTH,
        ];
        expected.sort_unstable();
        assert_eq!(lengths, expected, "{facts:?}");
        for (disk, sectors) in &sizes {
            assert!(
                restored[disk] * 8 < *sectors,
                "{upper:?}: the clone read {} of the {sectors} sectors of {disk}",
                restored[disk]
            );
        }

        // A read of each disk to its end, which the statistics count.
        let facts = cloned.probe(&["read-disks"]);
        let read = disks(&facts["read-disks.disks"]);
        let whole = sizes
            .iter()
            .map(|(disk, sectors)| (disk.clone(), sectors * SECTOR))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(read, whole, "{facts:?}");
        let facts = cloned.probe(&["reads"]);
        let after = disks(&facts["reads.disks"]);
        for (disk, sectors) in &sizes {
            assert!(
                after[disk] >= restored[disk] + sectors / 2,
                "{upper:?}: {disk} read {} sectors, then {}, of {sectors}",
                restored[disk],
                after[disk]
            );
        }
        cloned.stop();
    }
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_clone_s_time_settles_after_it_is_ready() {
    let template = Template::create("cl-time");
    let mut cloned = template.start("cl-t", StartOf::default(), 0);
    cloned.assert_started();
    let deadline = Instant::now() + TIME_SETTLEMENT;
    let facts = loop {
        let facts = cloned.probe(&["time"]);
        let status = facts.get("time.restore-status").map(String::as_str);
        if !matches!(status, None | Some("pending")) {
            break facts;
        }
        assert!(Instant::now() < deadline, "{facts:?}");
        thread::sleep(Duration::from_millis(100));
    };
    let settled = [
        "restore-status",
        "violations",
        "last-downtime-source",
        "restore-generation",
    ]
    .map(|key| &facts[&format!("time.{key}")]);
    let generation = &facts["time.generation"];
    assert_eq!(
        settled,
        ["ok", "0", "monotonic", generation.as_str()],
        "{facts:?}"
    );
    let downtime = facts["time.last-downtime-ns"].parse::<u64>().unwrap();
    assert!(downtime > 0, "{facts:?}");

    // The clone still serves its order.
    let facts = cloned.probe(&["activation", "memory"]);
    assert_timeline(&facts);
    assert_memory(&facts, TEMPLATE_MEMORY);
    cloned.stop();
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn each_command_reattaches_to_the_clone_from_a_new_process() {
    let template = Template::create("cl-again");
    let start = StartOf {
        upper: Upper::Scratch,
        ..StartOf::default()
    };
    let mut cloned = template.start("cl-g", start, 0);
    cloned.assert_started();
    // The launch committed: the sandbox records the clone's VM, and how far the clone got, and no
    // failure.
    let store = SandboxStore::open(&cloned.sandbox.store).unwrap();
    let records = CloneRecords::read(&store).unwrap().unwrap();
    assert_eq!(records.lease.id().to_string(), cloned.started["lease"]);
    assert_eq!(records.progress, Progress::Committed);
    assert!(records.runtime.is_some(), "{records:?}");
    assert!(records.journal.is_none() && records.failure.is_none());

    // Each command, from a new process, reattaches to the clone in a new session of the same
    // broker, in which the clone answers the Activate of its record again with its Ready, without
    // activating again, so that the events of its activation stay as they were.
    let first = cloned.sandbox.run("reattach", &[], 0);
    assert_eq!(first["reattached"], "live", "{first:?}");
    assert_eq!(first["pid"], cloned.started["pid"]);
    let facts = cloned.probe(&["activation"]);
    let events = assert_timeline(&facts);
    let ran = cloned.exec(&["--identity", NOBODY, "--", "/bin/sh", "-c", "id -u"]);
    let facts_ran = ["launch", "status", "stdout"].map(|key| &ran[&format!("exec.1.{key}")]);
    assert_eq!(facts_ran, ["started", "exit 0", "65534\\n"], "{ran:?}");
    let last = cloned.sandbox.run("reattach", &[], 0);
    let epochs = [&first, &facts, &ran, &last].map(|printed| {
        assert_eq!(printed["instance"], first["instance"], "{printed:?}");
        printed["epoch"].parse::<u64>().unwrap()
    });
    assert!(
        epochs.windows(2).all(|pair| pair[0] < pair[1]),
        "{epochs:?}"
    );
    let facts = cloned.probe(&["activation"]);
    let again = assert_timeline(&facts);
    assert_eq!(again, events, "{facts:?}");
    let execution = facts["activation.execution"].parse::<u64>().unwrap();
    assert!(execution >= events[3], "{facts:?}");
    cloned.stop();
    let store = SandboxStore::open(&cloned.sandbox.store).unwrap();
    assert!(CloneRecords::read(&store).unwrap().is_none());
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_host_program_of_another_runtime_ends_the_clone_through_its_verified_identity() {
    let template = Template::create("cl-other");
    let cloned = template.start("cl-o", StartOf::default(), 0);
    cloned.assert_started();
    let vmm = cloned.sandbox.vmm().expect("the clone's VMM runs");
    let mut sandbox = cloned.sandbox;

    // The reattach finds the clone's VMM under another runtime identity, which might not speak
    // this host's protocol, so it ends it through its verified identity, without a session, and
    // the clone fails, by the host's evidence, which quarantines nothing.
    let options = [OsStr::new("--guest-build"), OsStr::new(ANOTHER_BUILD)];
    let ended = sandbox.run("reattach", &options, CLONE_FAILED);
    assert_eq!(ended["reattached"], "failed", "{ended:?}");
    assert_eq!(
        failure(&ended),
        [
            "host",
            "runtime-mismatch",
            "reattach",
            "committed",
            "none",
            "terminated"
        ],
        "{ended:?}"
    );
    assert_eq!(ended["failure.code"], "runtime-mismatch");
    assert_eq!(ended["quarantined"], "no");
    assert_eq!(ended["released"], cloned.started["lease"], "{ended:?}");
    assert!(!ended.contains_key("epoch"), "{ended:?}");
    assert!(sandbox.stderr().contains("differs in initramfs"));
    assert!(
        vmm.wait(Instant::now() + Duration::from_secs(10)).unwrap(),
        "the clone's VMM still runs"
    );
    assert!(sandbox.record().is_none());
    let store = SandboxStore::open(&sandbox.store).unwrap();
    assert!(CloneRecords::read(&store).unwrap().is_none());
    assert!(template.leases().is_empty());

    // The template still serves clones.
    let mut another = template.start("cl-p", StartOf::default(), 0);
    another.assert_started();
    another.stop();
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_clone_whose_vm_exits_once_a_stop_found_it_fails_before_its_lease_is_released() {
    let template = Template::create("cl-gone");
    let cloned = template.start("cl-x", StartOf::default(), 0);
    cloned.assert_started();
    let vmm = cloned.sandbox.vmm().expect("the clone's VMM runs");
    let mut sandbox = cloned.sandbox;

    // The stop finds the clone running, in a new session, and pauses there, while the clone's VMM
    // ends without a word of its guest, as a crash of the VMM would end it.
    let pause = sandbox.root.join("found.pause");
    let hook = OsString::from(
        StepHook {
            step: Step::Found,
            action: Action::Pause(pause.clone()),
        }
        .to_string(),
    );
    let mut stopping = sandbox.start("stop", &[], &[(STEP_HOOK, &hook)]);
    assert_eq!(sandbox.paused(&mut stopping, &pause), "found");
    vmm.kill().unwrap();
    assert!(
        vmm.wait(Instant::now() + Duration::from_secs(10)).unwrap(),
        "the clone's VMM still runs"
    );
    fs::remove_file(&pause).unwrap();

    // The stop stops nothing, and the clone's VM ended without its holder, with no report of the
    // guest's own end: the clone fails, as NVX attributes the loss of its status, which
    // quarantines nothing, before the stop releases the lease.
    let ended = sandbox.finish(stopping, CLONE_FAILED);
    let found = ["reattached", "pid", "stopped"].map(|key| &ended[key]);
    assert_eq!(
        found,
        ["live", cloned.started["pid"].as_str(), "exited"],
        "{ended:?}"
    );
    assert_eq!(
        failure(&ended),
        [
            "unknown",
            "lost-status",
            "run",
            "committed",
            "none",
            "exited"
        ],
        "{ended:?}"
    );
    assert_eq!(ended["failure.code"], "exited");
    assert_eq!(ended["quarantined"], "no");
    assert_eq!(ended["released"], cloned.started["lease"], "{ended:?}");
    let stderr = sandbox.stderr();
    assert!(
        stderr.contains("without an outcome report of the guest's own end"),
        "{stderr}"
    );
    assert!(sandbox.record().is_none());
    let store = SandboxStore::open(&sandbox.store).unwrap();
    assert!(CloneRecords::read(&store).unwrap().is_none());
    assert!(template.leases().is_empty());
    assert!(template.store().tombstones().unwrap().is_empty());

    // The template still serves clones.
    let mut another = template.start("cl-y", StartOf::default(), 0);
    another.assert_started();
    another.stop();
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn a_tombstone_leaves_the_running_clone_of_its_generation_to_its_lease() {
    let template = Template::create("cl-tomb");
    let snapshot = template.payload().join(SNAPSHOT_NAME);
    let captured = snapshot_files(&snapshot);
    let start = StartOf {
        upper: Upper::Scratch,
        ..StartOf::default()
    };
    let mut cloned = template.start("cl-q", start, 0);
    cloned.assert_started();
    let store = template.store();
    let deadline = Instant::now() + Duration::from_secs(10);
    store.tombstone(GENERATION, Some(deadline)).unwrap();
    assert_eq!(store.tombstones().unwrap(), [GENERATION]);

    // The generation takes no new lease.
    let refused = template.start("cl-n", StartOf::default(), FAILED);
    assert!(refused.started.is_empty(), "{:?}", refused.started);
    refused.assert_refused(&format!(
        "a tombstone quarantines the generation {GENERATION} of the template store"
    ));

    // The clone keeps running on its lease: it reattaches, serves its order, and stops, which
    // releases the lease.
    let facts = cloned.probe(&["activation", "memory"]);
    assert_timeline(&facts);
    assert_memory(&facts, TEMPLATE_MEMORY);
    let ran = cloned.exec(&["--identity", NOBODY, "--", "/bin/true"]);
    assert_eq!(ran["exec.1.status"], "exit 0", "{ran:?}");
    assert_eq!(template.leases().len(), 1);
    cloned.stop();
    assert!(template.leases().is_empty());

    // The tombstone changed nothing of the generation's snapshot.
    assert_eq!(snapshot_files(&snapshot), captured);
    assert_payload(&template, false);
}

#[test]
#[ignore = "boots VMs; see the module documentation for what they need"]
fn the_lease_of_a_start_that_ended_before_its_record_is_released_through_its_template_store() {
    let template = Template::create("cl-lost");
    // A start that ended once the store leased the generation, before it recorded the clone,
    // leaves the lease, and no record that names the template store.
    let mut sandbox = Sandbox::new("cl-l");
    template.stage(&mut sandbox);
    let lease = lease_to(&template, &sandbox);

    // A sandbox that holds a lease must not move. A command on one that moved refuses it, rather
    // than take for it the sandbox that the lease's path holds then: one with a lease of its own,
    // then one without a lease.
    let named = sandbox.store.clone();
    let moved = sandbox.root.join("moved");
    fs::rename(&named, &moved).unwrap();
    SandboxStore::create(&named).unwrap();
    let other = lease_to(&template, &sandbox);
    sandbox.store = moved.clone();
    let refused = sandbox.run("stop", &[], FAILED);
    assert!(refused.is_empty(), "{refused:?}");
    let stderr = sandbox.stderr();
    assert!(stderr.contains("which does not hold it"), "{stderr}");
    assert_eq!(template.leases().len(), 2);
    sandbox.store = named.clone();
    let options = [OsStr::new("--template"), template.dir().as_os_str()];
    let released = sandbox.run("stop", &options, FAILED);
    assert_eq!(released["released"], other, "{released:?}");
    sandbox.store = moved.clone();
    let refused = sandbox.run("reattach", &[], FAILED);
    assert!(refused.is_empty(), "{refused:?}");
    let stderr = sandbox.stderr();
    assert!(stderr.contains("which does not hold it"), "{stderr}");
    let leases = template.leases();
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].id().to_string(), lease);
    // The sandbox moves back, as its lease names it.
    fs::remove_dir_all(&named).unwrap();
    fs::rename(&moved, &named).unwrap();
    sandbox.store = named;

    // The stop finds that the clone never launched, and keeps the lease without its template
    // store.
    let kept = sandbox.run("stop", &[], FAILED);
    assert_eq!(kept["reattached"], "ended", "{kept:?}");
    assert_eq!(kept["progress"], "planned", "{kept:?}");
    assert!(!kept.contains_key("released"), "{kept:?}");
    let stderr = sandbox.stderr();
    assert!(stderr.contains("records no clone"), "{stderr}");
    assert!(stderr.contains("--template TEMPLATES"), "{stderr}");
    assert_eq!(template.leases().len(), 1);

    // Another template store does not release it.
    let other = sandbox.root.join("other");
    Store::create(&other).unwrap();
    let options = [OsStr::new("--template"), other.as_os_str()];
    let refused = sandbox.run("stop", &options, FAILED);
    assert!(!refused.contains_key("released"), "{refused:?}");
    let stderr = sandbox.stderr();
    assert!(stderr.contains("not of this one"), "{stderr}");
    assert_eq!(template.leases().len(), 1);

    // Its own does, and the sandbox leases the generation again, to a clone whose record the
    // release then leaves.
    let options = [OsStr::new("--template"), template.dir().as_os_str()];
    let released = sandbox.run("reattach", &options, FAILED);
    assert_eq!(released["released"], lease, "{released:?}");
    assert!(template.leases().is_empty());
    let started = start_in(&mut sandbox, &template, template.dir());
    assert_ne!(started["lease"], lease, "{started:?}");
    let stopped = sandbox.run("stop", &[], 0);
    assert_eq!(stopped["released"], started["lease"], "{stopped:?}");
    assert!(template.leases().is_empty());

    // A start of another template's clone in the sandbox that ended as the first did leaves a
    // lease of that template's store, which the earlier clone's record does not name.
    let another = Template::create("cl-gone");
    let lease = lease_to(&another, &sandbox);
    let kept = sandbox.run("stop", &[], FAILED);
    assert!(!kept.contains_key("released"), "{kept:?}");
    let stderr = sandbox.stderr();
    assert!(stderr.contains("which did not grant its lease"), "{stderr}");
    assert!(stderr.contains("--template TEMPLATES"), "{stderr}");
    assert_eq!(another.leases().len(), 1);
    let options = [OsStr::new("--template"), another.dir().as_os_str()];
    let released = sandbox.run("stop", &options, FAILED);
    assert_eq!(released["released"], lease, "{released:?}");
    assert!(another.leases().is_empty());
    let started = start_in(&mut sandbox, &template, another.dir());
    let stopped = sandbox.run("stop", &[], 0);
    assert_eq!(stopped["released"], started["lease"], "{stopped:?}");
    assert!(another.leases().is_empty());
    assert_payload(&template, false);
    assert_payload(&another, false);
}

/// Leases the generation of `template` to the store of `sandbox`, through NVX's store, as a start
/// does before it records its clone, and returns the lease's identity.
fn lease_to(template: &Template, sandbox: &Sandbox) -> String {
    let now = HostTime::now().unwrap();
    let acquired = template
        .store()
        .acquire(GENERATION, &sandbox.store, &CallerFiles::none(), &now, None)
        .unwrap();
    acquired.lease.id().to_string()
}

/// Starts a clone in RAM, in the store of `sandbox`, which staged the image of `template`, of the
/// generation of the template store `templates`, checks that it started, and returns what the
/// start printed.
fn start_in(sandbox: &mut Sandbox, template: &Template, templates: &Path) -> Printed {
    let generation = GENERATION.to_string();
    let distro = erofs(&template.image);
    let options = [
        OsStr::new("--template"),
        templates.as_os_str(),
        OsStr::new("--generation"),
        OsStr::new(&generation),
        OsStr::new("--distro"),
        &distro,
        OsStr::new("--distro-identity"),
        OsStr::new(&template.identity),
        OsStr::new("--upper"),
        OsStr::new("ram"),
    ];
    sandbox.run("start", &options, 0)
}
