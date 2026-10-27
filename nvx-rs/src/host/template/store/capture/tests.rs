//! Tests of the captures of templates and of their publication, whose VMs a source in this
//! process emulates as OpenVMM runs them: captures of a class and the generations that they
//! publish, the checks of their files before their VMs start and of what their VMs leave, their
//! failures and crashes around each commit, and their recovery. The tests of
//! `tests/template_capture.rs` run captures through the supervisor, with the fake OpenVMM.

use std::cell::{Cell, RefCell};
use std::fs::OpenOptions;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use serde_json::{Value, json};

use super::*;
use crate::host::sandbox::distro::{DISTRO_POLICY_REVISION, PolicyVerdict};
use crate::host::sandbox::{SandboxDistro, SandboxUpper};
use crate::host::store::{CallerFiles, JOURNAL_NAME, LaunchId};
use crate::host::template::eligibility::MAX_DOWNTIME_NS;
use crate::host::template::store::Lease;
use crate::host::template::store::tests::{
    cause, link_dir, link_file, private_dir, publish_with, storage, store,
};
use crate::host::template::test_support::{MIB, ManifestSpec, class, network_class};
use crate::protocol::root::RootImage;
use crate::uuid::Uuid;

/// Bytes in a GiB.
const GIB: u64 = 1024 * MIB;

/// Nanoseconds in a second.
const SECOND: u64 = 1_000_000_000;

/// The identity of the placeholder.
const IDENTITY: &str = "11111111-1111-1111-1111-111111111111";

/// The access control list that OpenVMM gives a snapshot's directory and each of its files on
/// Windows: SYSTEM and the owner alone, and nothing that files inherit.
#[cfg(windows)]
const OPENVMM_ACCESS: &str = "D:P(A;;FA;;;SY)(A;;FA;;;OW)";

/// Returns a deadline that no capture of these tests reaches.
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(60)
}

/// Returns the host's time `seconds` after the captures of [`ManifestSpec::template`], on the
/// host and in the boot that captured them.
fn after(seconds: u64) -> HostTime {
    let manifest = TemplateManifest::parse(&ManifestSpec::template().encode()).unwrap();
    let time = manifest.time();
    HostTime {
        host_id: time.host_id,
        boot_id: time.boot_id,
        clock: time.host_clock,
        monotonic_ns: time.capture_monotonic_ns + seconds * SECOND,
        utc_ns: time.capture_utc_ns + seconds * SECOND,
    }
}

/// Returns the host's time `seconds` before the captures of [`ManifestSpec::template`], at
/// which none of them is eligible yet.
fn before(seconds: u64) -> HostTime {
    let anchor = after(0);
    HostTime {
        monotonic_ns: anchor.monotonic_ns - seconds * SECOND,
        utc_ns: anchor.utc_ns - seconds * SECOND,
        ..anchor
    }
}

/// Creates the sparse file `path` of `length` bytes, which no one else may write, whatever the
/// umask.
fn sparse(path: &Path, length: u64) {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options.open(path).unwrap();
    platform::make_sparse(&file).unwrap();
    file.set_len(length).unwrap();
}

/// Lets anyone change the file or directory `path`.
fn let_anyone_write(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = if path.is_dir() { 0o777 } else { 0o666 };
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }
    #[cfg(windows)]
    platform::set_access(path, "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)(A;OICI;FA;;;WD)").unwrap();
}

/// Returns the names of the entries of `dir`, sorted.
fn sorted(dir: &Path) -> Vec<String> {
    let mut names = entries(dir).unwrap();
    names.sort();
    names
}

/// Returns the file at `path` as JSON.
fn json_of(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

/// What the emulated VM of a capture does, as OpenVMM would: it stages a snapshot of the
/// manifest that it publishes beside the destination, with the RAM as a link to the file that
/// backed it, renames it to the destination, and writes the outcome report.
#[derive(Clone)]
struct Emulation {
    /// The manifest of the snapshot that it publishes, if it publishes one.
    manifest: Option<ManifestSpec>,
    /// The operation, category, and status code that its outcome report records, if it writes
    /// one.
    outcome: Option<(&'static str, &'static str, i64)>,
    /// An entry that it publishes in the snapshot besides its artifacts.
    extra: Option<&'static str>,
    /// An artifact that it leaves out of the snapshot.
    missing: Option<&'static str>,
    /// The length of the saved state that it publishes, if not the manifest's.
    state_len: Option<u64>,
    /// Whether it leaves the file that backed the RAM, and a staging directory, as OpenVMM leaves
    /// them when it ends abruptly.
    leftovers: bool,
}

impl Emulation {
    /// Returns the emulation of a capture of [`ManifestSpec::template`] that succeeds.
    fn capture() -> Self {
        Self {
            manifest: Some(ManifestSpec::template()),
            outcome: Some(("capture", "success", 0)),
            extra: None,
            missing: None,
            state_len: None,
            leftovers: false,
        }
    }

    /// Returns the emulation of a VM whose guest failed with 3 before it captured.
    fn guest_failure() -> Self {
        Self {
            manifest: None,
            outcome: Some(("capture", "guest-exit", 3)),
            ..Self::capture()
        }
    }

    /// Runs the VM of `vm` from the sandbox store of `locked`, the capture's transaction, after
    /// removing the outcome report of the store's previous VM, as a launch does.
    fn run(&self, locked: &mut LockedStore, vm: &Vm<'_>) -> Result<()> {
        // OpenVMM requires the control socket of a VM that captures a snapshot, on Linux, beside
        // the snapshot's destination: the VM launches from its transaction.
        assert_eq!(locked.dir(), vm.template.transaction());
        assert_eq!(
            locked.socket_path().parent(),
            vm.template.destination().parent()
        );
        remove_if_present(&locked.outcome_path())?;
        let transaction = vm.template.transaction();
        let ram = transaction.join(format!("{MEMORY_FILE_PREFIX}emulated"));
        sparse(&ram, vm.capture.class.memory);
        if let Some(spec) = &self.manifest {
            let staging = transaction.join(format!("{STAGING_PREFIX}1-0-0"));
            private_dir(&staging);
            fs::write(staging.join(MANIFEST_NAME), spec.encode()).unwrap();
            sparse(
                &staging.join(STATE_NAME),
                self.state_len.unwrap_or(spec.state_size),
            );
            fs::hard_link(&ram, staging.join(MEMORY_NAME)).unwrap();
            if let Some(extra) = self.extra {
                fs::write(staging.join(extra), b"").unwrap();
            }
            if let Some(missing) = self.missing {
                fs::remove_file(staging.join(missing)).unwrap();
            }
            #[cfg(windows)]
            for name in entries(&staging).unwrap() {
                platform::set_access(&staging.join(name), OPENVMM_ACCESS).unwrap();
            }
            #[cfg(windows)]
            platform::set_access(&staging, OPENVMM_ACCESS).unwrap();
            fs::rename(&staging, vm.template.destination()).unwrap();
        }
        if self.leftovers {
            private_dir(&transaction.join(format!("{STAGING_PREFIX}1-0-1")));
        } else {
            fs::remove_file(&ram).unwrap();
        }
        if let Some((operation, category, status_code)) = self.outcome {
            let report = json!({
                "schema_version": 1,
                "backend": vm.capture.class.hypervisor.as_str(),
                "outcome": {
                    "operation": operation,
                    "category": category,
                    "status_code": status_code,
                },
                "teardown": { "vm_stopped": true },
            });
            fs::write(locked.outcome_path(), report.to_string()).unwrap();
        }
        Ok(())
    }
}

/// A store, and the inputs of captures into it, of [`class`], whose VMs [`Emulation`]s run.
struct Bench {
    /// The temporary directory that holds the store and the platform's files.
    _directory: tempfile::TempDir,
    /// The store.
    store: Store,
    /// The inputs of a capture.
    capture: Capture,
    /// The number of VMs that captures ran.
    runs: Cell<usize>,
}

impl Bench {
    /// Returns a bench whose placeholder is a regular file, in a directory private to the current
    /// user.
    fn new() -> Self {
        Self::with(Path::to_path_buf)
    }

    /// Returns a bench whose placeholder is at the path that `prepare` returns, given a sparse
    /// file of 1 GiB in a directory private to the current user, which it may change before the
    /// placeholder's binding is made.
    fn with(prepare: impl FnOnce(&Path) -> PathBuf) -> Self {
        let (directory, store) = store();
        let platform = directory.path().join("platform");
        private_dir(&platform);
        let image = platform.join("placeholder.erofs");
        sparse(&image, GIB);
        let placeholder = prepare(&image);
        let path = placeholder.to_str().unwrap();
        let layer = SandboxLayer::new(LayerRole::Distro, path, IDENTITY);
        let binding = DistroBinding::import(&layer, PolicyVerdict::ACCEPTED).unwrap();
        let image = RootImage::erofs(Some(Uuid::parse(IDENTITY).unwrap()));
        let upper = SandboxUpper::scratch(platform.join("clone.ext4").to_str().unwrap());
        let capture = Capture {
            class: class(),
            adapter: SandboxPlan::adapter(SandboxDistro::new(path, image), upper),
            binding,
            policy_revision: DISTRO_POLICY_REVISION,
            artifacts: Artifacts::new(
                platform.join("openvmm"),
                platform.join("vmlinux"),
                platform.join("initramfs.cpio.gz"),
            ),
        };
        Self {
            _directory: directory,
            store,
            capture,
            runs: Cell::new(0),
        }
    }

    /// Returns the slot of the class of the captures.
    fn slot(&self) -> PathBuf {
        self.store
            .captures_dir()
            .join(slot_name(self.capture.class.identity().unwrap()))
    }

    /// Returns the transaction of the captures.
    fn transaction(&self) -> PathBuf {
        self.slot().join(TRANSACTION_NAME)
    }

    /// Serves the capture as [`Store::create_template`] does with the host's time that `now`
    /// returns, or, without it, as [`Store::refresh_template`] does, with `emulation` as its VM,
    /// failing it at the points at which `interrupt` fails, and crashing it if `crash`.
    fn serve(
        &self,
        now: Option<&dyn Fn() -> Result<HostTime>>,
        emulation: &Emulation,
        interrupt: &dyn Fn(Point) -> Result<()>,
        crash: bool,
    ) -> Result<Created> {
        self.serve_with(
            now,
            &|locked, vm| emulation.run(locked, vm),
            interrupt,
            crash,
        )
    }

    /// Serves the capture as [`serve`](Self::serve) does, with `run` as its source.
    fn serve_with(
        &self,
        now: Option<&dyn Fn() -> Result<HostTime>>,
        run: &dyn Fn(&mut LockedStore, &Vm<'_>) -> Result<()>,
        interrupt: &dyn Fn(Point) -> Result<()>,
        crash: bool,
    ) -> Result<Created> {
        let counted = |locked: &mut LockedStore, vm: &Vm<'_>| {
            self.runs.set(self.runs.get() + 1);
            run(locked, vm)
        };
        let hooks = Hooks {
            sync: &sync_dir,
            run: &counted,
            interrupt,
            crash,
        };
        self.store.capture(&self.capture, now, deadline(), &hooks)
    }

    /// Serves the capture as [`Store::create_template`] does, a second after the captures.
    fn create(&self, emulation: &Emulation) -> Result<Created> {
        self.serve(Some(&|| Ok(after(1))), emulation, &uninterrupted, false)
    }

    /// Serves the capture as [`Store::refresh_template`] does.
    fn refresh(&self, emulation: &Emulation) -> Result<Generation> {
        self.serve(None, emulation, &uninterrupted, false)
            .map(Created::into_generation)
    }

    /// Refreshes the capture, failing it at `point`, or crashing it there if `crash`, and returns
    /// the error.
    fn interrupted(&self, point: Point, crash: bool) -> Error {
        let interrupt = |at: Point| {
            if at == point {
                Err(Error::other(format!("failed at {at:?}")))
            } else {
                Ok(())
            }
        };
        self.serve(None, &Emulation::capture(), &interrupt, crash)
            .unwrap_err()
    }

    /// Recovers the store's captures.
    fn recover(&self) -> Result<Vec<Generation>> {
        self.recover_syncing(&sync_dir)
    }

    /// Recovers the store's captures, syncing directories through `sync`.
    fn recover_syncing(&self, sync: SyncDir<'_>) -> Result<Vec<Generation>> {
        let run = |_: &mut LockedStore, _: &Vm<'_>| -> Result<()> {
            unreachable!("a recovery runs no VM")
        };
        let hooks = Hooks {
            sync,
            run: &run,
            interrupt: &uninterrupted,
            crash: false,
        };
        self.store.recover_with(deadline(), &hooks)
    }

    /// Returns the store generations that the index lists.
    fn indexed(&self) -> Vec<u64> {
        numbers(self.store.index().unwrap().generations())
    }

    /// Drops the index's last generation and keeps its payload, as a crash leaves a publication
    /// that renamed its envelope into the store before it committed the index. Captures complete
    /// such publications before they run, so tests that need one beside a checked transaction
    /// make it this way.
    fn unindex_last(&self) {
        let mut index = self.store.index().unwrap();
        let last = index.generations.pop().unwrap();
        index.next = last.generation();
        self.store.write_index(&index).unwrap();
    }
}

/// Returns the store generation `generation`, which must not be 0.
fn store_generation(generation: u64) -> StoreGeneration {
    StoreGeneration::new(std::num::NonZeroU64::new(generation).unwrap())
}

/// Returns the store generations of `generations`, as numbers.
fn numbers(generations: &[Generation]) -> Vec<u64> {
    generations
        .iter()
        .map(|generation| generation.generation().get().get())
        .collect()
}

/// Returns the files that the sandboxes of these tests keep: a configuration record.
fn files() -> CallerFiles {
    CallerFiles::new("config.json").unwrap()
}

/// Provisions the sandbox `name` in `parent`, and returns its directory.
fn provision(parent: &Path, name: &str) -> PathBuf {
    let dir = parent.join(name);
    let locked = SandboxStore::lock_for_provision(&dir, &files(), None).unwrap();
    fs::write(dir.join("config.json"), b"{}").unwrap();
    drop(locked);
    dir
}

/// Asserts that the slot of `bench` holds no transaction, and records no launch.
fn assert_clean(bench: &Bench) {
    assert!(!bench.transaction().exists());
    assert!(!bench.slot().join(JOURNAL_NAME).exists());
}

#[test]
fn captures_publish_generations_that_creations_then_serve() {
    let bench = Bench::new();
    let Created::Captured(first) = bench.create(&Emulation::capture()).unwrap() else {
        panic!("the first creation of a class captures it");
    };
    assert_eq!(bench.runs.get(), 1);
    assert_eq!(first.generation(), store_generation(1));
    let class = class();
    let manifest = TemplateManifest::parse(&ManifestSpec::template().encode()).unwrap();
    let key = TemplateKey::new(&class, &manifest).unwrap();
    assert_eq!(first.key(), key.identity());
    assert_eq!(first.class(), class.identity().unwrap());
    assert_eq!(first.created_at(), manifest.created_at());
    assert_eq!(first.time(), manifest.time());
    assert_eq!(
        (first.state_len(), first.memory_len()),
        (manifest.state_len(), manifest.memory_len())
    );
    assert_eq!(
        bench.store.index().unwrap().generations(),
        std::slice::from_ref(&first)
    );
    // The payload is the envelope: OpenVMM's snapshot, with the capture's record and the
    // template's key beside it.
    let payload = bench.store.payload_dir(first.generation());
    assert_eq!(
        sorted(&payload),
        [CAPTURE_RECORD_NAME, KEY_NAME, SNAPSHOT_NAME]
    );
    assert_eq!(sorted(&payload.join(SNAPSHOT_NAME)), SNAPSHOT_ARTIFACTS);
    assert_eq!(bench.store.read_key(&first).unwrap(), key);
    assert_eq!(
        bench.store.snapshot_dir(&first).unwrap(),
        payload.join(SNAPSHOT_NAME)
    );
    assert_eq!(
        json_of(&payload.join(CAPTURE_RECORD_NAME)),
        json!({
            "format": STORE_FORMAT,
            "store": bench.store.id().to_string(),
            "class": class.identity().unwrap().to_string(),
            "storageGeneration": first.storage_generation().to_string(),
        })
    );
    assert_clean(&bench);
    // A creation serves the class with the generation that it holds, and runs no VM.
    assert_eq!(
        bench.create(&Emulation::capture()).unwrap(),
        Created::Existing(first.clone())
    );
    assert_eq!(bench.runs.get(), 1);
    // A refresh always captures, with a fresh storage generation, and creations then serve the
    // new generation.
    let second = bench.refresh(&Emulation::capture()).unwrap();
    assert_eq!(bench.runs.get(), 2);
    assert_eq!(second.generation(), store_generation(2));
    assert_ne!(second.storage_generation(), first.storage_generation());
    assert_eq!(second.key(), first.key());
    assert_eq!(
        bench.create(&Emulation::capture()).unwrap(),
        Created::Existing(second.clone())
    );
    assert_eq!(bench.indexed(), [1, 2]);
    assert_eq!(bench.store.read_key(&first).unwrap(), key);
}

#[test]
fn captures_limit_the_processors_that_their_class_keeps_offline_at_boot() {
    let mut bench = Bench::new();
    bench.capture.class.processors = 4;
    bench.capture.class.boot_processors = 1;
    // OpenVMM records the processors online at boot that the command line limits.
    let mut spec = ManifestSpec::template();
    spec.vp_count = 4;
    let contract = spec.contract();
    contract.topology = (1, 1, 4, 1, vec![0, 1, 2, 3]);
    contract.boot_online_vp_count = 1;
    let emulation = Emulation {
        manifest: Some(spec),
        ..Emulation::capture()
    };
    let arguments = RefCell::new(Vec::new());
    let run = |locked: &mut LockedStore, vm: &Vm<'_>| {
        let command = vm.command("control", Path::new("outcome.json"), locked.dir())?;
        arguments.replace(command.arguments);
        emulation.run(locked, vm)
    };
    let created = bench.serve_with(Some(&|| Ok(after(1))), &run, &uninterrupted, false);
    let Created::Captured(generation) = created.unwrap() else {
        panic!("the first creation of a class captures it");
    };
    // The VM has the class's processors, and boots with those that it keeps online.
    let arguments: Vec<String> = arguments
        .take()
        .into_iter()
        .map(|argument| argument.into_string().unwrap())
        .collect();
    let value = |option: &str| {
        let at = arguments.iter().position(|given| given == option).unwrap();
        arguments[at + 1].as_str()
    };
    assert_eq!(value("--processors"), "4");
    assert_eq!(value("--cmdline"), "nvx_sandbox=1 maxcpus=1");
    let key = bench.store.read_key(&generation).unwrap();
    assert_eq!((key.processors(), key.boot_processors()), (4, 1));
}

#[test]
fn refreshes_leave_older_generations_and_their_leases_untouched() {
    let bench = Bench::new();
    let first = bench.refresh(&Emulation::capture()).unwrap();
    let sandboxes = tempfile::tempdir().unwrap();
    let sandbox = provision(sandboxes.path(), "clone");
    let lease = bench
        .store
        .acquire(first.generation(), &sandbox, &files(), &after(1), None)
        .unwrap()
        .lease;
    let payload = bench.store.payload_dir(first.generation());
    let before = (sorted(&payload), bench.store.read_key(&first).unwrap());
    let second = bench.refresh(&Emulation::capture()).unwrap();
    assert_eq!(bench.indexed(), [1, 2]);
    assert_eq!(
        bench.store.index().unwrap().get(first.generation()),
        Some(&first)
    );
    assert_eq!(
        (sorted(&payload), bench.store.read_key(&first).unwrap()),
        before
    );
    assert_eq!(
        bench.store.leases(first.generation()).unwrap(),
        std::slice::from_ref(&lease)
    );
    assert!(bench.store.leases(second.generation()).unwrap().is_empty());
    assert_eq!(
        Lease::of(&SandboxStore::open(&sandbox).unwrap()).unwrap(),
        Some(lease)
    );
}

#[test]
fn creations_capture_unless_the_class_has_a_generation_that_restores_take() {
    let bench = Bench::new();
    // A generation of another class serves none of this one's.
    let other = publish_with(
        &bench.store,
        &network_class(),
        &ManifestSpec::with_network(),
        storage(9),
    );
    let Created::Captured(first) = bench.create(&Emulation::capture()).unwrap() else {
        panic!("a class without generations is captured");
    };
    assert_eq!(first.generation(), store_generation(2));
    // A quarantined generation serves no creation.
    bench.store.tombstone(first.generation(), None).unwrap();
    let Created::Captured(second) = bench.create(&Emulation::capture()).unwrap() else {
        panic!("a class whose generations are quarantined is captured");
    };
    assert_eq!(second.generation(), store_generation(3));
    // Neither does one that no restore takes at the host's time.
    let late = HostTime {
        monotonic_ns: after(0).monotonic_ns + MAX_DOWNTIME_NS + SECOND,
        ..after(0)
    };
    assert!(!second.eligibility(&late).is_eligible());
    let Created::Captured(third) = bench
        .serve(
            Some(&|| Ok(late)),
            &Emulation::capture(),
            &uninterrupted,
            false,
        )
        .unwrap()
    else {
        panic!("a class whose generations are ineligible is captured");
    };
    assert_eq!(third.generation(), store_generation(4));
    assert_eq!(
        bench.create(&Emulation::capture()).unwrap(),
        Created::Existing(third)
    );
    assert_eq!(bench.runs.get(), 3);
    assert_eq!(
        bench.store.index().unwrap().get(other.generation()),
        Some(&other)
    );
}

#[test]
fn failures_before_the_check_retire_the_transaction() {
    let bench = Bench::new();
    for (point, runs) in [(Point::Prepared, 0), (Point::Exited, 1)] {
        let error = bench.interrupted(point, false);
        assert_eq!(error.message(), format!("failed at {point:?}"));
        assert_eq!(bench.runs.replace(0), runs, "{point:?}");
        assert_clean(&bench);
    }
    let failures = [
        (
            Emulation::guest_failure(),
            ErrorKind::Exited,
            "records the outcome guest-exit with status 3 of the operation capture",
        ),
        // OpenVMM may fail once it has published the snapshot, as when it cannot make the
        // publication durable.
        (
            Emulation {
                outcome: Some(("capture", "vmm-failure", 1)),
                ..Emulation::capture()
            },
            ErrorKind::Exited,
            "records the outcome vmm-failure with status 1",
        ),
        (
            Emulation {
                outcome: None,
                ..Emulation::capture()
            },
            ErrorKind::Exited,
            "ended without the outcome report",
        ),
        (
            Emulation {
                outcome: Some(("run", "success", 0)),
                ..Emulation::capture()
            },
            ErrorKind::Exited,
            "of the operation run",
        ),
        (
            Emulation {
                manifest: None,
                ..Emulation::capture()
            },
            ErrorKind::Io,
            "cannot open the snapshot of the capture",
        ),
        (
            Emulation {
                missing: Some(MANIFEST_NAME),
                ..Emulation::capture()
            },
            ErrorKind::Io,
            "cannot read the snapshot manifest",
        ),
        (
            Emulation {
                missing: Some(MEMORY_NAME),
                ..Emulation::capture()
            },
            ErrorKind::Io,
            "holds manifest.bin, state.bin, not exactly the manifest",
        ),
        (
            Emulation {
                manifest: Some(ManifestSpec::with_network()),
                ..Emulation::capture()
            },
            ErrorKind::Io,
            "the capture does not match its class",
        ),
        (
            Emulation {
                extra: Some("scratch.img"),
                ..Emulation::capture()
            },
            ErrorKind::Io,
            "manifest.bin, memory.bin, scratch.img, state.bin, not exactly the manifest",
        ),
        (
            Emulation {
                state_len: Some(ManifestSpec::template().state_size - 512),
                ..Emulation::capture()
            },
            ErrorKind::Io,
            "bytes that its manifest records",
        ),
    ];
    for (emulation, kind, message) in failures {
        let error = bench.refresh(&emulation).unwrap_err();
        assert_eq!(error.kind(), kind, "{error}");
        assert!(error.message().contains(message), "{error}");
        assert_eq!(bench.runs.replace(0), 1);
        assert_clean(&bench);
        // The slot keeps the outcome report of the latest capture, and none of an earlier one.
        assert_eq!(
            bench.slot().join(OUTCOME_NAME).exists(),
            emulation.outcome.is_some()
        );
    }
    assert!(bench.indexed().is_empty());
}

#[test]
fn what_openvmm_leaves_beside_the_snapshot_is_removed() {
    let bench = Bench::new();
    let emulation = Emulation {
        leftovers: true,
        ..Emulation::capture()
    };
    let generation = bench.refresh(&emulation).unwrap();
    assert_eq!(
        sorted(&bench.store.payload_dir(generation.generation())),
        [CAPTURE_RECORD_NAME, KEY_NAME, SNAPSHOT_NAME]
    );
    assert_clean(&bench);
}

#[test]
fn checked_transactions_outlive_failures_until_recovery_publishes_them() {
    let bench = Bench::new();
    bench.interrupted(Point::Checked, false);
    assert_eq!(
        sorted(&bench.transaction()),
        [CAPTURE_RECORD_NAME, KEY_NAME, SNAPSHOT_NAME]
    );
    assert!(bench.indexed().is_empty());
    let published = bench.recover().unwrap();
    assert_eq!(published.len(), 1);
    assert_eq!(bench.store.index().unwrap().generations(), published);
    assert_clean(&bench);
    // The next capture of the class publishes it too, and then serves it.
    let bench = Bench::new();
    bench.interrupted(Point::Checked, false);
    let Created::Existing(generation) = bench.create(&Emulation::capture()).unwrap() else {
        panic!("a creation serves the generation that it recovered");
    };
    assert_eq!(generation.generation(), store_generation(1));
    assert_eq!(bench.runs.get(), 1);
    assert_clean(&bench);
}

#[test]
fn publications_interrupted_once_they_renamed_their_envelope_are_completed() {
    let bench = Bench::new();
    bench.interrupted(Point::Renamed, false);
    assert!(bench.indexed().is_empty());
    assert_eq!(
        sorted(&bench.store.payload_dir(store_generation(1))),
        [CAPTURE_RECORD_NAME, KEY_NAME, SNAPSHOT_NAME]
    );
    // The next capture completes it first.
    let generation = bench.refresh(&Emulation::capture()).unwrap();
    assert_eq!(generation.generation(), store_generation(2));
    assert_eq!(bench.indexed(), [1, 2]);
    // So does a recovery.
    bench.interrupted(Point::Renamed, false);
    let published = bench.recover().unwrap();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].generation(), store_generation(3));
    assert_eq!(bench.indexed(), [1, 2, 3]);
    // A recovery that completes one before it publishes a checked transaction returns both.
    bench.interrupted(Point::Checked, false);
    bench.unindex_last();
    assert_eq!(bench.indexed(), [1, 2]);
    assert_eq!(numbers(&bench.recover().unwrap()), [3, 4]);
    assert_eq!(bench.indexed(), [1, 2, 3, 4]);
    assert_clean(&bench);
}

#[test]
fn recoveries_make_what_they_publish_durable_before_they_index_it() {
    let bench = Bench::new();
    bench.refresh(&Emulation::capture()).unwrap();
    bench.interrupted(Point::Checked, false);
    bench.unindex_last();
    // Each directory that the recovery syncs, with what the index lists as it does.
    let synced = RefCell::new(Vec::new());
    let sync = |dir: &Path| -> Result<()> {
        synced
            .borrow_mut()
            .push((dir.to_path_buf(), bench.indexed()));
        sync_dir(dir)
    };
    assert_eq!(numbers(&bench.recover_syncing(&sync).unwrap()), [1, 2]);
    let generations = bench.store.dir.join(GENERATIONS_NAME);
    let parent = bench.store.dir.parent().unwrap().to_path_buf();
    assert_eq!(
        synced.into_inner(),
        [
            // The transaction that the crash left checked.
            (bench.transaction(), vec![]),
            // The publication that the crash interrupted once it had renamed its envelope.
            (bench.store.payload_dir(store_generation(1)), vec![]),
            (generations.clone(), vec![]),
            (bench.slot(), vec![]),
            // The transaction's publication.
            (generations, vec![]),
            (bench.slot(), vec![]),
            (bench.store.dir.clone(), vec![1, 2]),
            (parent, vec![1, 2]),
        ]
    );
    assert_clean(&bench);
}

#[test]
fn captures_make_the_store_durable_before_they_report_a_generation() {
    let bench = Bench::new();
    // Each directory that the capture syncs, with what the index lists as it does.
    let synced = RefCell::new(Vec::new());
    let sync = |dir: &Path| -> Result<()> {
        synced
            .borrow_mut()
            .push((dir.to_path_buf(), bench.indexed()));
        sync_dir(dir)
    };
    let run = |locked: &mut LockedStore, vm: &Vm<'_>| Emulation::capture().run(locked, vm);
    let hooks = Hooks {
        sync: &sync,
        run: &run,
        interrupt: &uninterrupted,
        crash: false,
    };
    let created = bench
        .store
        .capture(&bench.capture, None, deadline(), &hooks)
        .unwrap();
    assert_eq!(created.generation().generation(), store_generation(1));
    let parent = bench.store.dir.parent().unwrap().to_path_buf();
    assert_eq!(
        synced.into_inner(),
        [
            // The checked transaction, and every directory above it, up to the store's parent,
            // which the store's creation did not sync.
            (bench.transaction(), vec![]),
            (bench.slot(), vec![]),
            (bench.store.captures_dir(), vec![]),
            (bench.store.dir.clone(), vec![]),
            (parent.clone(), vec![]),
            // The rename into the store.
            (bench.store.dir.join(GENERATIONS_NAME), vec![]),
            (bench.slot(), vec![]),
            // The index.
            (bench.store.dir.clone(), vec![1]),
            (parent, vec![1]),
        ]
    );
}

#[test]
fn creations_recover_the_index_before_they_look_for_a_generation() {
    // A creation completes a publication that a crash interrupted, and serves its generation,
    // the class's latest.
    let bench = Bench::new();
    bench.refresh(&Emulation::capture()).unwrap();
    bench.interrupted(Point::Renamed, false);
    assert_eq!(bench.indexed(), [1]);
    let Created::Existing(generation) = bench.create(&Emulation::capture()).unwrap() else {
        panic!("a creation serves the generation whose publication it completed");
    };
    assert_eq!(generation.generation(), store_generation(2));
    assert_eq!(bench.indexed(), [1, 2]);
    assert_eq!(bench.runs.get(), 2);
    // It serves no generation whose payload a crash lost, and captures one in its place.
    let bench = Bench::new();
    bench.refresh(&Emulation::capture()).unwrap();
    fs::remove_dir_all(bench.store.payload_dir(store_generation(1))).unwrap();
    let Created::Captured(generation) = bench.create(&Emulation::capture()).unwrap() else {
        panic!("a creation captures in place of a generation whose payload was lost");
    };
    assert_eq!(generation.generation(), store_generation(2));
    assert_eq!(bench.indexed(), [2]);
    // A store whose index is full of generations whose payloads a crash lost still captures.
    let bench = Bench::new();
    let first = bench.refresh(&Emulation::capture()).unwrap();
    let last = MAX_GENERATIONS as u64;
    let mut index = bench.store.index().unwrap();
    index
        .generations
        .extend((2..=last).map(|generation| Generation {
            generation: store_generation(generation),
            ..first.clone()
        }));
    index.next = store_generation(last + 1);
    bench.store.write_index(&index).unwrap();
    assert_eq!(bench.indexed().len(), MAX_GENERATIONS);
    let generation = bench.refresh(&Emulation::capture()).unwrap();
    assert_eq!(generation.generation(), store_generation(last + 1));
    assert_eq!(bench.indexed(), [1, last + 1]);
    assert_clean(&bench);
}

#[test]
fn recoveries_drop_lost_generations_before_they_index_another() {
    // A store whose index is full of generations that a crash lost, but one, beside a publication
    // that a crash interrupted once it had renamed its envelope.
    let bench = Bench::new();
    let first = bench.refresh(&Emulation::capture()).unwrap();
    bench.interrupted(Point::Renamed, false);
    let last = MAX_GENERATIONS as u64;
    fs::rename(
        bench.store.payload_dir(store_generation(2)),
        bench.store.payload_dir(store_generation(last + 1)),
    )
    .unwrap();
    let mut index = bench.store.index().unwrap();
    index
        .generations
        .extend((2..=last).map(|generation| Generation {
            generation: store_generation(generation),
            ..first.clone()
        }));
    index.next = store_generation(last + 1);
    bench.store.write_index(&index).unwrap();
    assert_eq!(bench.indexed().len(), MAX_GENERATIONS);
    // The lost generations take none of the store's capacity from the publication.
    assert_eq!(numbers(&bench.recover().unwrap()), [last + 1]);
    assert_eq!(bench.indexed(), [1, last + 1]);
    assert_clean(&bench);
}

#[test]
fn creations_make_the_store_durable_before_they_serve_a_generation() {
    let bench = Bench::new();
    let parent = bench.store.dir.parent().unwrap().to_path_buf();
    let run = |locked: &mut LockedStore, vm: &Vm<'_>| Emulation::capture().run(locked, vm);
    // A capture whose host crashes as it syncs the store's parent, once the index lists the
    // capture's generation.
    let crashing = |dir: &Path| -> Result<()> {
        if dir == parent && !bench.indexed().is_empty() {
            return Err(Error::other("the host crashed"));
        }
        sync_dir(dir)
    };
    let hooks = Hooks {
        sync: &crashing,
        run: &run,
        interrupt: &uninterrupted,
        crash: true,
    };
    bench
        .store
        .capture(&bench.capture, None, deadline(), &hooks)
        .unwrap_err();
    assert_eq!(bench.indexed(), [1]);
    // A creation serves the generation once it has made the index, and the store's own entry in
    // its parent, durable, although no recovery changed the index.
    let synced = RefCell::new(Vec::new());
    let sync = |dir: &Path| -> Result<()> {
        synced.borrow_mut().push(dir.to_path_buf());
        sync_dir(dir)
    };
    let hooks = Hooks {
        sync: &sync,
        run: &run,
        interrupt: &uninterrupted,
        crash: false,
    };
    let now = || -> Result<HostTime> { Ok(after(1)) };
    let Created::Existing(generation) = bench
        .store
        .capture(&bench.capture, Some(&now), deadline(), &hooks)
        .unwrap()
    else {
        panic!("a creation serves the generation that the index lists");
    };
    assert_eq!(generation.generation(), store_generation(1));
    assert_eq!(synced.into_inner(), [bench.store.dir.clone(), parent]);
    assert_clean(&bench);
}

#[test]
fn recovery_ends_the_captures_that_crashes_interrupt() {
    for (point, left, indexed) in [
        (
            Point::Prepared,
            vec![CAPTURE_RECORD_NAME, LOCK_NAME, SCRATCH_NAME],
            vec![],
        ),
        (
            Point::Exited,
            vec![CAPTURE_RECORD_NAME, LOCK_NAME, SCRATCH_NAME, SNAPSHOT_NAME],
            vec![],
        ),
        (
            Point::Checked,
            vec![CAPTURE_RECORD_NAME, KEY_NAME, SNAPSHOT_NAME],
            vec![1],
        ),
        (Point::Renamed, vec![], vec![1]),
        (Point::Indexed, vec![], vec![1]),
    ] {
        let bench = Bench::new();
        bench.interrupted(point, true);
        if left.is_empty() {
            assert!(!bench.transaction().exists(), "{point:?}");
        } else {
            assert_eq!(sorted(&bench.transaction()), left, "{point:?}");
        }
        let published = bench.recover().unwrap();
        let recovered: Vec<u64> = published
            .iter()
            .map(|generation| generation.generation().get().get())
            .collect();
        let expected = if matches!(point, Point::Checked | Point::Renamed) {
            vec![1]
        } else {
            vec![]
        };
        assert_eq!(recovered, expected, "{point:?}");
        assert_eq!(bench.indexed(), indexed, "{point:?}");
        assert_clean(&bench);
        // The slot captures again.
        bench.refresh(&Emulation::capture()).unwrap();
    }
}

#[test]
fn generations_whose_payloads_are_lost_are_dropped_from_the_index() {
    let bench = Bench::new();
    let first = bench.refresh(&Emulation::capture()).unwrap();
    bench.refresh(&Emulation::capture()).unwrap();
    fs::remove_dir_all(bench.store.payload_dir(first.generation())).unwrap();
    bench.refresh(&Emulation::capture()).unwrap();
    assert_eq!(bench.indexed(), [2, 3]);
    fs::remove_dir_all(bench.store.payload_dir(store_generation(3))).unwrap();
    assert!(bench.recover().unwrap().is_empty());
    assert_eq!(bench.indexed(), [2]);
    // The index never assigns a store generation twice.
    assert_eq!(
        bench.store.index().unwrap().next_generation(),
        store_generation(4)
    );
}

#[test]
fn a_transaction_stays_while_it_records_a_launch() {
    let bench = Bench::new();
    // A source that fails while its launch remains, as one whose VMM may still run: dropping a
    // launch leaves its journal, as a crash would.
    let failed = |locked: &mut LockedStore, vm: &Vm<'_>| -> Result<()> {
        let capability = [7; crate::protocol::CAPABILITY_LEN];
        let launch = LaunchId::generate()?;
        drop(locked.begin_launch(&launch, &vm.capture.class.runtime, &capability)?);
        Err(Error::other("the VMM may still run"))
    };
    let error = bench
        .serve_with(None, &failed, &uninterrupted, false)
        .unwrap_err();
    assert_eq!(error.message(), "the VMM may still run");
    // The transaction's sandbox store records the launch, and the slot none.
    assert!(bench.transaction().join(JOURNAL_NAME).exists());
    assert!(!bench.slot().join(JOURNAL_NAME).exists());
    let left = sorted(&bench.transaction());
    for name in [CAPTURE_RECORD_NAME, SCRATCH_NAME, LOCK_NAME, LOG_NAME] {
        assert!(left.iter().any(|left| left == name), "{name}: {left:?}");
    }
    // The next capture ends the launch, which nothing holds, retires the transaction, and
    // captures.
    bench.refresh(&Emulation::capture()).unwrap();
    assert_clean(&bench);
    assert_eq!(bench.indexed(), [1]);
}

/// Returns whether this host lets the tests create a symbolic link to a file: Windows lets only
/// Developer Mode or a privilege create one.
fn can_link_files() -> bool {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("target");
    fs::write(&target, b"").unwrap();
    link_file(&target, &directory.path().join("link"))
}

/// Asserts that `result` failed with `kind`, with a message that holds `message`, and that its
/// capture ran no VM, and left no transaction.
fn assert_refused(bench: &Bench, result: Result<Created>, kind: ErrorKind, message: &str) {
    let error = result.unwrap_err();
    assert_eq!(error.kind(), kind, "{error}");
    assert!(
        error.message().contains(message) || cause(&error).contains(message),
        "{error}: {}",
        cause(&error)
    );
    assert_eq!(bench.runs.replace(0), 0);
    assert!(bench.indexed().is_empty());
}

#[test]
fn links_and_unsafe_ownership_are_refused_before_the_vm_starts() {
    // A placeholder that is a link, which OpenVMM would not follow, though the check of its
    // binding follows it.
    if can_link_files() {
        let bench = Bench::with(|image| {
            let link = image.with_file_name("link.erofs");
            assert!(link_file(image, &link));
            link
        });
        let result = bench.create(&Emulation::capture());
        assert_refused(
            &bench,
            result,
            ErrorKind::Unavailable,
            "it is a link, or not a regular file",
        );
        assert_clean(&bench);
    }
    // A placeholder that others may change.
    let bench = Bench::with(|image| {
        let_anyone_write(image);
        image.to_path_buf()
    });
    let result = bench.create(&Emulation::capture());
    let message = if cfg!(windows) {
        "allows other users to change it"
    } else {
        "is writable by other users"
    };
    assert_refused(&bench, result, ErrorKind::Unavailable, message);
    assert_clean(&bench);
    // A scratch that a link replaced, or that others may change.
    let bench = Bench::new();
    let scratch = bench.transaction().join(SCRATCH_NAME);
    let replaced = |point: Point| -> Result<()> {
        if point == Point::Prepared {
            let target = bench.slot().join("elsewhere.raw");
            sparse(&target, class().scratch.length);
            fs::remove_file(&scratch).unwrap();
            assert!(link_file(&target, &scratch));
        }
        Ok(())
    };
    if can_link_files() {
        let result = bench.serve(None, &Emulation::capture(), &replaced, false);
        assert_refused(
            &bench,
            result,
            ErrorKind::Io,
            "it is a link, or not a regular file",
        );
        assert_clean(&bench);
    }
    let writable = |point: Point| -> Result<()> {
        if point == Point::Prepared {
            let_anyone_write(&scratch);
        }
        Ok(())
    };
    let result = bench.serve(None, &Emulation::capture(), &writable, false);
    assert_refused(&bench, result, ErrorKind::Io, message);
    assert_clean(&bench);
    // A transaction that others may access, which the capture keeps rather than retire.
    let exposed_message = if cfg!(windows) {
        "allows other users"
    } else {
        "is accessible to other users"
    };
    let transaction = bench.transaction();
    let exposed = |point: Point| -> Result<()> {
        if point == Point::Prepared {
            let_anyone_write(&transaction);
        }
        Ok(())
    };
    let result = bench.serve(None, &Emulation::capture(), &exposed, false);
    assert_refused(&bench, result, ErrorKind::Io, exposed_message);
    assert!(transaction.exists());
    let result = bench.create(&Emulation::capture());
    assert_refused(&bench, result, ErrorKind::Io, exposed_message);
    fs::remove_dir_all(&transaction).unwrap();
    // A transaction that is a link.
    let elsewhere = bench.slot().join("elsewhere");
    private_dir(&elsewhere);
    link_dir(&elsewhere, &transaction);
    let result = bench.create(&Emulation::capture());
    assert_refused(&bench, result, ErrorKind::Io, "is not a plain directory");
    assert!(fs::symlink_metadata(&transaction).is_ok());
}

#[test]
fn scratches_are_the_files_that_their_captures_created() {
    // Another file of the scratch's length, renamed over the scratch. It is created while the
    // scratch exists, so that the host cannot give it the scratch's identity again.
    let bench = Bench::new();
    let scratch = bench.transaction().join(SCRATCH_NAME);
    let replaced = |point: Point| -> Result<()> {
        if point == Point::Prepared {
            let other = bench.slot().join("other.raw");
            sparse(&other, class().scratch.length);
            fs::rename(&other, &scratch).unwrap();
        }
        Ok(())
    };
    let result = bench.serve(None, &Emulation::capture(), &replaced, false);
    assert_refused(
        &bench,
        result,
        ErrorKind::Io,
        "is not the file that the capture created",
    );
    assert_clean(&bench);
    // A file whose ID differs from the scratch's only above its low 64 bits, as the 128-bit IDs
    // of two files of a file system such as ReFS may: the check compares the whole ID.
    let inputs = Inputs::check(&bench.capture).unwrap();
    let template = TemplatePlan::new(
        &bench.capture.adapter,
        &bench.capture.binding,
        bench.capture.class.scratch.length,
        inputs.resources,
        &bench.transaction(),
        StorageGeneration::generate().unwrap(),
    )
    .unwrap();
    private_dir(&bench.transaction());
    let created = create_scratch(template.scratch_path(), template.scratch_length()).unwrap();
    check_scratch(&template, created).unwrap();
    let (device, file_id) = created;
    let error = check_scratch(&template, (device, file_id ^ (1 << 64))).unwrap_err();
    assert!(
        error
            .message()
            .contains("is not the file that the capture created"),
        "{error}"
    );
}

/// Returns whether this process holds the file at `path` open for writing.
#[cfg(target_os = "linux")]
fn held_for_writing(path: &Path) -> bool {
    let target = fs::canonicalize(path).unwrap();
    fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(std::result::Result::ok)
        .any(|entry| {
            let flags = || {
                let info =
                    fs::read_to_string(Path::new("/proc/self/fdinfo").join(entry.file_name()))
                        .ok()?;
                let flags = info.lines().find_map(|line| line.strip_prefix("flags:"))?;
                u32::from_str_radix(flags.trim(), 8).ok()
            };
            fs::read_link(entry.path()).is_ok_and(|linked| linked == target)
                && flags().is_some_and(|flags| flags & 0o3 != 0)
        })
}

/// Opens the file at `path` as OpenVMM opens a block device of a capture on Windows: without
/// following a reparse point, for reading, and for writing if `write`, sharing reads and
/// deletion alone, which fails while another handle may write it.
#[cfg(windows)]
fn open_as_openvmm(path: &Path, write: bool) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;

    /// `FILE_SHARE_READ | FILE_SHARE_DELETE`.
    const SHARE_READ_DELETE: u32 = 0x1 | 0x4;
    /// `FILE_FLAG_OPEN_REPARSE_POINT`.
    const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    OpenOptions::new()
        .read(true)
        .write(write)
        .share_mode(SHARE_READ_DELETE)
        .custom_flags(OPEN_REPARSE_POINT)
        .open(path)
}

#[test]
fn no_handle_through_which_the_host_could_write_the_block_devices_is_open_as_the_vm_starts() {
    let bench = Bench::new();
    let checked = |locked: &mut LockedStore, vm: &Vm<'_>| -> Result<()> {
        #[cfg(any(target_os = "linux", windows))]
        let (placeholder, scratch) = (
            vm.template.plan().distro().path(),
            vm.template.scratch_path(),
        );
        #[cfg(target_os = "linux")]
        {
            assert!(!held_for_writing(placeholder));
            assert!(!held_for_writing(scratch));
        }
        #[cfg(windows)]
        {
            open_as_openvmm(placeholder, false).unwrap();
            open_as_openvmm(scratch, true).unwrap();
            // The checked placeholder stays held, so that nothing writes it meanwhile.
            let error = OpenOptions::new()
                .write(true)
                .open(placeholder)
                .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(32), "{error}");
        }
        Emulation::capture().run(locked, vm)
    };
    bench
        .serve_with(None, &checked, &uninterrupted, false)
        .unwrap();
    assert_eq!(bench.runs.get(), 1);
    // The placeholder is released once the VM has ended.
    OpenOptions::new()
        .write(true)
        .open(bench.capture.adapter.distro().path())
        .unwrap();
}

#[test]
fn captures_of_one_class_are_serialized() {
    let active = AtomicUsize::new(0);
    let most = AtomicUsize::new(0);
    let runs = AtomicUsize::new(0);
    let run = |locked: &mut LockedStore, vm: &Vm<'_>| -> Result<()> {
        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
        most.fetch_max(now, Ordering::SeqCst);
        runs.fetch_add(1, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(100));
        let ran = Emulation::capture().run(locked, vm);
        active.fetch_sub(1, Ordering::SeqCst);
        ran
    };
    // Serves the capture of `bench` twice at once, as creations at the host's time that `now`
    // returns, or as refreshes.
    let race =
        |bench: &Bench, now: Option<&(dyn Fn() -> Result<HostTime> + Sync)>| -> Vec<Created> {
            let (store, capture) = (&bench.store, &bench.capture);
            let barrier = Arc::new(Barrier::new(2));
            let serve = || {
                let hooks = Hooks {
                    sync: &sync_dir,
                    run: &run,
                    interrupt: &uninterrupted,
                    crash: false,
                };
                let now: Option<&dyn Fn() -> Result<HostTime>> = match now {
                    Some(now) => Some(now),
                    None => None,
                };
                barrier.wait();
                store.capture(capture, now, deadline(), &hooks).unwrap()
            };
            thread::scope(|scope| {
                let threads: Vec<_> = (0..2).map(|_| scope.spawn(serve)).collect();
                threads
                    .into_iter()
                    .map(|thread| thread.join().unwrap())
                    .collect()
            })
        };
    // Refreshes capture one after the other.
    let bench = Bench::new();
    let refreshed = race(&bench, None);
    assert!(
        refreshed
            .iter()
            .all(|created| matches!(created, Created::Captured(_)))
    );
    assert_eq!(runs.swap(0, Ordering::SeqCst), 2);
    assert_eq!(bench.indexed(), [1, 2]);
    // Creations that race make one generation between them, although both start before the
    // capture's anchor: the host's time is before it until a capture is published.
    let bench = Bench::new();
    let store = &bench.store;
    let now = || -> Result<HostTime> {
        Ok(if store.index()?.generations().is_empty() {
            before(1)
        } else {
            after(1)
        })
    };
    assert_eq!(now().unwrap(), before(1));
    let created = race(&bench, Some(&now));
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(created[0].generation(), created[1].generation());
    assert_eq!(
        created
            .iter()
            .filter(|created| matches!(created, Created::Captured(_)))
            .count(),
        1
    );
    assert_eq!(most.load(Ordering::SeqCst), 1);
}

#[test]
fn recoveries_refuse_entries_of_the_captures_that_are_not_slots() {
    let bench = Bench::new();
    assert!(bench.recover().unwrap().is_empty());
    bench.refresh(&Emulation::capture()).unwrap();
    fs::write(bench.store.captures_dir().join("notes.txt"), b"").unwrap();
    let error = bench.recover().unwrap_err();
    assert_eq!(error.code(), ErrorCode::ForeignFiles, "{error}");
    assert!(error.message().ends_with(": notes.txt"), "{error}");
}

#[cfg(target_os = "linux")]
#[test]
fn stores_too_deep_for_the_control_socket_of_a_capture_are_refused() {
    let bench = Bench::new();
    let parent = bench._directory.path().join("d".repeat(80));
    fs::create_dir(&parent).unwrap();
    let bench = Bench {
        store: Store::create(parent.join("templates")).unwrap(),
        ..bench
    };
    let error = bench.refresh(&Emulation::capture()).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
    assert!(
        error.message().contains("too deep for the control socket"),
        "{error}"
    );
    assert_eq!(bench.runs.get(), 0);
}
