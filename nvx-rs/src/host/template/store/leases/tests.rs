//! Tests of leases, clone bindings, tombstones, and retirement, by one process, and by
//! transactions in processes of their own, which [`transaction_role`] runs; and of the crashes of
//! a clone's orchestrator, which [`clone_role`] runs, with a [`vmm_role`] as the clone's VMM.

use std::cell::{Cell, RefCell};
use std::convert::Infallible;
use std::env;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use super::*;
use crate::activation::TemplateReference;
use crate::host::process::{Process, ProcessRef};
use crate::host::store::files::write_private;
use crate::host::store::{
    CLONE_ACTIVATION_NAME, CLONE_FAILURE_NAME, CLONE_PROGRESS_NAME, CLONE_SCRATCH_NAME, FORMAT,
    LOCK_NAME, LaunchState, RESTORE_READY_NAME, RUNTIME_NAME, Recovery, RuntimeIdentity,
};
use crate::host::supervisor::{self, Reattached, StopOutcome, VmmCommand};
use crate::host::template::eligibility::MAX_DOWNTIME_NS;
use crate::host::template::store::SNAPSHOT_NAME;
use crate::host::template::store::tests::{
    cause, link_dir, link_file, publish, publish_with, remove, storage, store as new_store,
    unlink_dir,
};
use crate::host::template::test_support::{ManifestSpec, artifact, network_class};
use crate::host::test_support::released;
use crate::protocol::CAPABILITY_LEN;

/// A change to a record, as JSON.
type Change = fn(&mut Value);

/// Nanoseconds in a second.
const SECOND: u64 = 1_000_000_000;
/// Nanoseconds in a day.
const DAY: u64 = 86_400 * SECOND;
/// The configuration record of these tests' sandboxes.
const CONFIG: &str = "config.json";
/// The directory of the store in which [`transaction_role`] runs its transaction.
const ROLE_STORE: &str = "NVX_TEMPLATE_LEASE_TEST_STORE";
/// The transaction that [`transaction_role`] runs: `acquire`, `release`, `tombstone`, or
/// `retire`.
const ROLE_ACTION: &str = "NVX_TEMPLATE_LEASE_TEST_ACTION";
/// The store generation of the transaction of [`transaction_role`].
const ROLE_GENERATION: &str = "NVX_TEMPLATE_LEASE_TEST_GENERATION";
/// The directory of the sandbox of the transaction of [`transaction_role`], if it has one.
const ROLE_SANDBOX: &str = "NVX_TEMPLATE_LEASE_TEST_SANDBOX";
/// The line that [`transaction_role`] prints once it is ready, before it waits for a line on its
/// standard input.
const READY: &str = "nvx-template-lease-test-ready";
/// The start of the line in which [`transaction_role`] prints how its transaction ended.
const OUTCOME: &str = "nvx-template-lease-test-outcome ";
/// The step after which [`clone_role`] crashes: `after-record`, `after-binding`, or `acquired`, of
/// its acquisition; `before-spawn`, `after-spawn`, `after-identity`, or `committed`, of its
/// clone's launch; or `releasing`, of a release.
const ROLE_CRASH: &str = "NVX_TEMPLATE_LEASE_TEST_CRASH";
/// The working directory of the VMM that [`clone_role`] launches.
const ROLE_VMM: &str = "NVX_TEMPLATE_LEASE_TEST_VMM";
/// The start of the line in which [`clone_role`] prints the reference of the VMM that it spawned,
/// as JSON.
const REFERENCE: &str = "nvx-template-lease-test-vmm ";
/// The start of the line that [`clone_role`] prints as it crashes, before the step after which it
/// crashes.
const CRASHED: &str = "nvx-template-lease-test-crashed ";
/// File whose presence in its working directory marks a [`vmm_role`]. Without it, the role
/// returns at once, as it does when `cargo test -- --ignored` runs it.
const VMM_MARKER: &str = "vmm-role";
/// File whose appearance in its working directory ends a [`vmm_role`].
const EXIT_MARKER: &str = "exit";
/// The capability of the VMMs of these tests' clones.
const CAPABILITY: [u8; CAPABILITY_LEN] = [7; CAPABILITY_LEN];
/// Longest wait for a VMM that is expected to exit, for a sandbox's lock, or for a reattach to
/// become determinate.
const TIMEOUT: Duration = Duration::from_secs(60);
/// Longest life of a [`vmm_role`] that nothing ends, so that a failed test leaves no process.
const VMM_LIFETIME: Duration = Duration::from_secs(180);

/// Returns the files that these tests' callers keep in their sandboxes.
fn files() -> CallerFiles {
    CallerFiles::new(CONFIG).unwrap()
}

/// Provisions the sandbox `name` in `parent`, and returns its directory, with its links resolved,
/// as a lease names it.
fn provision(parent: &Path, name: &str) -> PathBuf {
    let dir = parent.join(name);
    let locked = SandboxStore::lock_for_provision(&dir, &files(), None).unwrap();
    fs::write(dir.join(CONFIG), b"{}").unwrap();
    drop(locked);
    filesystem::canonicalize(&dir).unwrap()
}

/// Returns the time of the host that captured `generation`, during the boot of the capture,
/// `seconds` after it.
fn after(generation: &Generation, seconds: u64) -> HostTime {
    let time = generation.time();
    HostTime {
        host_id: time.host_id,
        boot_id: time.boot_id,
        clock: time.host_clock,
        monotonic_ns: time.capture_monotonic_ns + seconds * SECOND,
        utc_ns: time.capture_utc_ns + seconds * SECOND,
    }
}

/// Leases `generation` of `store` to the sandbox in `sandbox`, a second after its capture, and
/// releases the sandbox's lock.
fn leased(store: &Store, generation: &Generation, sandbox: &Path) -> Lease {
    store
        .acquire(
            generation.generation(),
            sandbox,
            &files(),
            &after(generation, 1),
            None,
        )
        .unwrap()
        .lease
}

/// Returns the error with which leasing `generation` of `store` to the sandbox in `sandbox` at
/// `now` fails.
fn refusal(store: &Store, generation: StoreGeneration, sandbox: &Path, now: &HostTime) -> Error {
    store
        .acquire(generation, sandbox, &files(), now, None)
        .unwrap_err()
}

/// Releases the lease of the sandbox in `sandbox` from `store`, under the sandbox's lock.
fn release(store: &Store, sandbox: &Path) -> Result<Option<Lease>> {
    let mut locked = SandboxStore::lock_existing(sandbox, &files(), None).unwrap();
    store.release(&mut locked)
}

/// Returns the lease that the clone binding of the sandbox in `sandbox` records, if any.
fn binding(sandbox: &Path) -> Option<Lease> {
    Lease::of(&SandboxStore::open(sandbox).unwrap()).unwrap()
}

/// Returns the store generation `generation`, which must not be 0.
fn store_generation(generation: u64) -> StoreGeneration {
    StoreGeneration::new(NonZeroU64::new(generation).unwrap())
}

/// Returns a runtime identity under this crate's contracts.
fn runtime() -> RuntimeIdentity {
    RuntimeIdentity::new(artifact('a'), artifact('b'), artifact('c'))
}

/// Records a VM of the sandbox in `sandbox`, as the commit of its launch does.
fn record_vm(sandbox: &Path) {
    let record = json!({
        "format": FORMAT,
        "launch": "0123456789abcdef0123456789abcdef",
        "process": { "pid": 42, "startTime": 4242, "boot": "ab".repeat(16) },
        "endpoint": "/state/sandbox/control.sock",
        "runtime": serde_json::to_value(runtime()).unwrap(),
    });
    write_private(&sandbox.join(RUNTIME_NAME), record.to_string().as_bytes()).unwrap();
}

/// Ends, through `locked`, a launch that spawned nothing, and whose log is closed.
///
/// A process that a concurrent test spawned may still hold the log, until it executes its
/// program, and a later recovery then ends the launch.
fn recover_unspawned(locked: &mut LockedStore) {
    assert!(released(|| locked.recover().unwrap() == Recovery::Exited));
}

/// Returns the file at `path` as JSON.
fn json_of(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

/// Returns the identities of `leases`, sorted, and separated by spaces.
fn ids(leases: &[Lease]) -> String {
    let mut ids: Vec<String> = leases.iter().map(|lease| lease.id().to_string()).collect();
    ids.sort();
    ids.join(" ")
}

/// Returns the arguments that run only the role `name` of this test executable, on libtest's main
/// thread, so that libtest prints the role's name before the role's own output.
fn role(name: &str) -> Vec<OsString> {
    [
        "--exact",
        &format!("host::template::store::leases::tests::{name}"),
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]
    .map(OsString::from)
    .to_vec()
}

/// Returns the error with which `locked` refuses to begin the launch `launch`.
fn launch_refusal(locked: &mut LockedStore, launch: &LaunchId) -> Error {
    match locked.begin_launch(launch, &runtime(), &CAPABILITY) {
        Ok(_) => panic!("the launch {launch} began"),
        Err(error) => error,
    }
}

#[test]
fn leases_bind_clones_to_the_generations_that_they_restore() {
    let (directory, store) = new_store();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let first = provision(directory.path(), "first");
    let acquired = store
        .acquire(
            published.generation(),
            &first,
            &files(),
            &after(&published, 1),
            None,
        )
        .unwrap();
    let lease = acquired.lease.clone();
    assert_eq!(lease.store(), store.id());
    assert_eq!(lease.generation(), published.generation());
    assert_eq!(lease.key(), published.key());
    assert_eq!(lease.sandbox(), first);
    assert_eq!(
        lease.template(),
        TemplateReference {
            generation: published.generation(),
            key: Identity::from(published.key()),
        }
    );
    // The caller keeps the sandbox's lock, under which it launches the clone, until it drops it.
    assert_eq!(acquired.locked.dir(), first);
    let busy = SandboxStore::lock_existing(&first, &files(), Some(Instant::now())).unwrap_err();
    assert_eq!(busy.code(), ErrorCode::SandboxBusy, "{busy}");
    drop(acquired);
    // The clone binding in the sandbox and the record in the store are the same lease.
    assert_eq!(binding(&first), Some(lease.clone()));
    assert_eq!(
        store.leases(published.generation()).unwrap(),
        std::slice::from_ref(&lease)
    );
    // A sandbox holds one lease at a time.
    let error = refusal(
        &store,
        published.generation(),
        &first,
        &after(&published, 2),
    );
    assert_eq!(error.code(), ErrorCode::SandboxLeased, "{error}");
    assert!(error.message().contains(&lease.id().to_string()), "{error}");
    // Other sandboxes lease the generation too, and other generations.
    let second = provision(directory.path(), "second");
    let mut leases = vec![lease, leased(&store, &published, &second)];
    leases.sort_by_key(Lease::id);
    let other = publish(&store, &ManifestSpec::template(), storage(2));
    let third = provision(directory.path(), "third");
    let other_lease = leased(&store, &other, &third);
    assert_eq!(store.leases(published.generation()).unwrap(), leases);
    assert_eq!(store.leases(other.generation()).unwrap(), [other_lease]);
}

#[test]
fn acquisitions_refuse_what_they_cannot_lease_and_lease_nothing() {
    let (directory, store) = new_store();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let generation = published.generation();
    let sandbox = provision(directory.path(), "sandbox");
    let now = after(&published, 1);
    // A generation that the store does not index.
    let error = refusal(&store, store_generation(9), &sandbox, &now);
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
    // One that is not eligible at the host's time: no longer, or not yet.
    let expired = after(&published, MAX_DOWNTIME_NS / SECOND + 1);
    let mut early = after(&published, 0);
    early.monotonic_ns -= 1;
    for (now, condition) in [
        (expired, "is no longer eligible"),
        (early, "is not eligible yet"),
    ] {
        let error = refusal(&store, generation, &sandbox, &now);
        assert_eq!(error.code(), ErrorCode::TemplateIneligible, "{error}");
        assert!(error.message().contains(condition), "{error}");
    }
    // On Linux, a payload that is not a directory of the current user with mode 0700, which a
    // launch of the clone requires, and through which a release ends the lease: it opens the
    // payload for reading, and removes the clone's socket from it. A user other than root cannot
    // open a payload of mode 0300 for reading at all.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::PermissionsExt;

        let payload = store.payload_dir(generation);
        let unopened = format!(
            "cannot open the payload of the generation {}",
            payload.display()
        );
        for mode in [0o300, 0o500] {
            fs::set_permissions(&payload, fs::Permissions::from_mode(mode)).unwrap();
            let error = refusal(&store, generation, &sandbox, &now);
            fs::set_permissions(&payload, fs::Permissions::from_mode(0o700)).unwrap();
            assert_eq!(error.kind(), ErrorKind::Io, "{error}");
            let refused = format!(
                "the payload of the generation {} has mode {mode:o}, not the mode 700 that the \
                 directory of a control socket must have",
                payload.display()
            );
            assert!(
                error.message() == refused || (mode == 0o300 && error.message() == unopened),
                "{error}"
            );
        }
        // A link in the place of the store's directory, of the directory of the generations, or of
        // the payload, even to a directory that holds the same files: a release, which follows
        // none of them, could not open the payload.
        let root = store.dir().to_owned();
        let generations = payload.parent().unwrap().to_owned();
        let elsewhere = directory.path().join("elsewhere");
        for linked in [&root, &generations, &payload] {
            fs::rename(linked, &elsewhere).unwrap();
            std::os::unix::fs::symlink(&elsewhere, linked).unwrap();
            let error = refusal(&store, generation, &sandbox, &now);
            fs::remove_file(linked).unwrap();
            fs::rename(&elsewhere, linked).unwrap();
            assert_eq!(error.kind(), ErrorKind::Io, "{error}");
            assert_eq!(error.message(), unopened, "{error}");
            let source = std::error::Error::source(&error).unwrap().to_string();
            assert!(
                source.ends_with("is a link, or not a directory"),
                "{source}"
            );
        }
    }
    // A sandbox that is not provisioned: its directory is missing, or holds no configuration.
    let unprovisioned = directory.path().join("unprovisioned");
    drop(SandboxStore::lock_for_provision(&unprovisioned, &files(), None).unwrap());
    for dir in [directory.path().join("missing"), unprovisioned] {
        let error = refusal(&store, generation, &dir, &now);
        assert_eq!(error.code(), ErrorCode::SandboxNotProvisioned, "{error}");
    }
    // A sandbox that records a launch, or a VM, which may still run.
    let mut locked = SandboxStore::lock_existing(&sandbox, &files(), None).unwrap();
    drop(
        locked
            .begin_launch(
                &LaunchId::generate().unwrap(),
                &runtime(),
                &[7; CAPABILITY_LEN],
            )
            .unwrap(),
    );
    drop(locked);
    let error = refusal(&store, generation, &sandbox, &now);
    assert_eq!(error.code(), ErrorCode::SandboxRunning, "{error}");
    let mut locked = SandboxStore::lock_existing(&sandbox, &files(), None).unwrap();
    recover_unspawned(&mut locked);
    record_vm(&sandbox);
    drop(locked);
    let error = refusal(&store, generation, &sandbox, &now);
    assert_eq!(error.code(), ErrorCode::SandboxRunning, "{error}");
    let mut locked = SandboxStore::lock_existing(&sandbox, &files(), None).unwrap();
    locked.clear_runtime().unwrap();
    drop(locked);
    // A generation that a tombstone quarantines.
    store.tombstone(generation, None).unwrap();
    let error = refusal(&store, generation, &sandbox, &now);
    assert_eq!(error.code(), ErrorCode::TemplateTombstoned, "{error}");
    // None of them leased anything.
    assert!(store.leases(generation).unwrap().is_empty());
    assert_eq!(binding(&sandbox), None);
    assert!(!store.dir().join(LEASES_NAME).exists());
}

#[test]
fn releases_end_leases_once_their_clones_have_exited() {
    let (directory, store) = new_store();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let generation = published.generation();
    let sandbox = provision(directory.path(), "sandbox");
    let mut acquired = store
        .acquire(generation, &sandbox, &files(), &after(&published, 1), None)
        .unwrap();
    let lease = acquired.lease.clone();
    // While a launch of the clone, or its VM, may run, its lease stays.
    drop(
        acquired
            .locked
            .begin_launch(lease.launch(), &runtime(), &[7; CAPABILITY_LEN])
            .unwrap(),
    );
    let error = store.release(&mut acquired.locked).unwrap_err();
    assert_eq!(error.code(), ErrorCode::SandboxRunning, "{error}");
    recover_unspawned(&mut acquired.locked);
    record_vm(&sandbox);
    let error = store.release(&mut acquired.locked).unwrap_err();
    assert_eq!(error.code(), ErrorCode::SandboxRunning, "{error}");
    assert_eq!(binding(&sandbox), Some(lease.clone()));
    assert_eq!(
        store.leases(generation).unwrap(),
        std::slice::from_ref(&lease)
    );
    // Once its VM has exited, the release removes the binding, and then the store's record.
    acquired.locked.clear_runtime().unwrap();
    assert_eq!(
        store.release(&mut acquired.locked).unwrap(),
        Some(lease.clone())
    );
    drop(acquired);
    assert_eq!(binding(&sandbox), None);
    assert!(store.leases(generation).unwrap().is_empty());
    assert_eq!(release(&store, &sandbox).unwrap(), None);
    // The sandbox may lease the generation again, under another identity.
    let again = leased(&store, &published, &sandbox);
    assert_ne!(again.id(), lease.id());
    // Another store's release keeps the lease.
    let (_other_directory, other) = new_store();
    let error = release(&other, &sandbox).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Other, "{error}");
    assert_eq!(binding(&sandbox), Some(again.clone()));
    assert_eq!(store.leases(generation).unwrap(), [again]);
}

#[test]
fn a_lease_allows_one_launch_of_its_clone_under_the_lock_that_acquired_it() {
    let (directory, store) = new_store();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let sandbox = provision(directory.path(), "sandbox");
    let mut acquired = store
        .acquire(
            published.generation(),
            &sandbox,
            &files(),
            &after(&published, 1),
            None,
        )
        .unwrap();
    let reserved = acquired.lease.launch().clone();
    let once =
        format!("whose launch {reserved} begins once, under the lock that acquired the lease");
    // No launch but the reserved one.
    let other = LaunchId::generate().unwrap();
    let error = launch_refusal(&mut acquired.locked, &other);
    assert_eq!(error.code(), ErrorCode::SandboxLeased, "{error}");
    assert!(
        error.message().ends_with(&format!(
            "which reserves the launch {reserved}, not {other}"
        )),
        "{error}"
    );
    // The reserved launch, which its journal names, once.
    drop(
        acquired
            .locked
            .begin_launch(&reserved, &runtime(), &CAPABILITY)
            .unwrap(),
    );
    assert_eq!(
        acquired.locked.journal().unwrap().unwrap().launch(),
        &reserved
    );
    recover_unspawned(&mut acquired.locked);
    for launch in [&reserved, &other] {
        let error = launch_refusal(&mut acquired.locked, launch);
        assert_eq!(error.code(), ErrorCode::SandboxLeased, "{error}");
    }
    let error = launch_refusal(&mut acquired.locked, &reserved);
    assert!(error.message().ends_with(&once), "{error}");
    // Once a release ends the lease, the sandbox launches as any launch.
    assert_eq!(
        store.release(&mut acquired.locked).unwrap(),
        Some(acquired.lease.clone())
    );
    drop(
        acquired
            .locked
            .begin_launch(&other, &runtime(), &CAPABILITY)
            .unwrap(),
    );
    recover_unspawned(&mut acquired.locked);
    drop(acquired);
    // The launch of a lease whose acquisition's lock is gone, as a crash leaves it, begins under
    // no other lock.
    let lease = leased(&store, &published, &sandbox);
    let mut locked = SandboxStore::lock_existing(&sandbox, &files(), None).unwrap();
    let error = launch_refusal(&mut locked, lease.launch());
    assert_eq!(error.code(), ErrorCode::SandboxLeased, "{error}");
    assert!(
        error.message().ends_with(&format!(
            "whose launch {} begins once, under the lock that acquired the lease",
            lease.launch()
        )),
        "{error}"
    );
}

#[test]
fn tombstones_quarantine_generations_and_keep_their_leases() {
    let (directory, store) = new_store();
    let first = publish(&store, &ManifestSpec::template(), storage(1));
    let second = publish(&store, &ManifestSpec::template(), storage(2));
    let leasing = provision(directory.path(), "leasing");
    let lease = leased(&store, &second, &leasing);
    let now = after(&second, 1);
    assert_eq!(
        store.select(second.key(), &now).unwrap(),
        Some(second.clone())
    );
    assert!(store.tombstones().unwrap().is_empty());
    // A tombstone stops new leases, and selection skips the generation, whose leases stay.
    store.tombstone(second.generation(), None).unwrap();
    store.tombstone(second.generation(), None).unwrap();
    assert_eq!(store.tombstones().unwrap(), [second.generation()]);
    let sandbox = provision(directory.path(), "sandbox");
    let error = refusal(&store, second.generation(), &sandbox, &now);
    assert_eq!(error.code(), ErrorCode::TemplateTombstoned, "{error}");
    assert_eq!(binding(&leasing), Some(lease.clone()));
    assert_eq!(store.leases(second.generation()).unwrap(), [lease]);
    assert_eq!(
        store.select(second.key(), &now).unwrap(),
        Some(first.clone())
    );
    leased(&store, &first, &sandbox);
    // A tombstone needs a generation that the store indexes.
    let error = store.tombstone(store_generation(9), None).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
    assert_eq!(store.tombstones().unwrap(), [second.generation()]);
}

#[test]
fn selection_takes_the_latest_eligible_generation_of_a_key() {
    let (_directory, store) = new_store();
    let first = publish(&store, &ManifestSpec::template(), storage(1));
    let mut spec = ManifestSpec::template();
    spec.time().capture_utc_ns += 10 * DAY;
    spec.time().capture_monotonic_ns += 10 * DAY;
    let second = publish(&store, &spec, storage(2));
    let other = publish_with(
        &store,
        &network_class(),
        &ManifestSpec::with_network(),
        storage(3),
    );
    assert_eq!(second.key(), first.key());
    assert_ne!(other.key(), first.key());
    // The host's time `days` after the first capture, during its boot.
    let at = |days: u64| HostTime {
        monotonic_ns: first.time().capture_monotonic_ns + days * DAY,
        utc_ns: first.time().capture_utc_ns + days * DAY,
        ..after(&first, 0)
    };
    let select = |now: &HostTime| store.select(first.key(), now).unwrap();
    // Before the second capture, only the first generation is eligible; then both are, and the
    // latest is selected; after the first's window, only the second is, and after both, none.
    assert_eq!(select(&at(1)), Some(first.clone()));
    assert_eq!(select(&at(11)), Some(second.clone()));
    assert_eq!(select(&at(35)), Some(second.clone()));
    assert_eq!(select(&at(45)), None);
    // A quarantined generation is skipped.
    store.tombstone(second.generation(), None).unwrap();
    assert_eq!(select(&at(11)), Some(first.clone()));
    assert_eq!(select(&at(35)), None);
    // Each key selects among its own generations.
    assert_eq!(store.select(other.key(), &at(1)).unwrap(), Some(other));
    assert_eq!(
        store
            .select(KeyIdentity::from_bytes([0; 32]), &at(1))
            .unwrap(),
        None
    );
}

#[test]
fn retirement_deletes_a_generation_once_its_leases_drain() {
    let (directory, store) = new_store();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let generation = published.generation();
    let sandbox = provision(directory.path(), "sandbox");
    let lease = leased(&store, &published, &sandbox);
    // The generation stays, quarantined, while its lease lasts.
    for _ in 0..2 {
        assert_eq!(
            store.retire(generation, None).unwrap(),
            Retirement::Draining(vec![lease.clone()])
        );
    }
    assert_eq!(store.tombstones().unwrap(), [generation]);
    assert_eq!(store.index().unwrap().get(generation), Some(&published));
    assert!(store.snapshot_dir(&published).is_ok());
    let other = provision(directory.path(), "other");
    let error = refusal(&store, generation, &other, &after(&published, 1));
    assert_eq!(error.code(), ErrorCode::TemplateTombstoned, "{error}");
    // Once its clone releases it, the generation goes: its payload, its entry, and its tombstone.
    assert_eq!(release(&store, &sandbox).unwrap(), Some(lease));
    assert_eq!(store.retire(generation, None).unwrap(), Retirement::Retired);
    assert!(!store.payload_dir(generation).exists());
    assert_eq!(store.index().unwrap().get(generation), None);
    assert!(store.tombstones().unwrap().is_empty());
    assert!(store.leases(generation).unwrap().is_empty());
    // Retiring it again finds it retired, and its store generation is never assigned again.
    assert_eq!(store.retire(generation, None).unwrap(), Retirement::Retired);
    let next = publish(&store, &ManifestSpec::template(), storage(1));
    assert!(next.generation() > generation);
    let unassigned = store_generation(next.generation().get().get() + 1);
    let error = store.retire(unassigned, None).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
    let error = refusal(&store, generation, &other, &after(&published, 1));
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
}

#[test]
fn retirement_removes_the_leases_that_no_clone_binds() {
    let (directory, store) = new_store();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let generation = published.generation();
    let sandboxes: Vec<PathBuf> = ["released", "removed", "leased-again", "bound"]
        .into_iter()
        .map(|name| provision(directory.path(), name))
        .collect();
    let leases: Vec<Lease> = sandboxes
        .iter()
        .map(|sandbox| leased(&store, &published, sandbox))
        .collect();
    // A release that was interrupted once it had removed the clone binding.
    fs::remove_file(sandboxes[0].join(CLONE_BINDING_NAME)).unwrap();
    // A sandbox whose removal removed its binding with the rest.
    let locked = SandboxStore::lock_existing(&sandboxes[1], &files(), None).unwrap();
    assert!(locked.remove().unwrap().is_none());
    assert!(!sandboxes[1].exists());
    // A sandbox that leased the generation again after such a release.
    fs::remove_file(sandboxes[2].join(CLONE_BINDING_NAME)).unwrap();
    let again = leased(&store, &published, &sandboxes[2]);
    let mut listed = leases.clone();
    listed.push(again.clone());
    listed.sort_by_key(Lease::id);
    assert_eq!(store.leases(generation).unwrap(), listed);
    // The retirement removes the records of the first three, and drains the bound ones.
    let mut live = vec![again, leases[3].clone()];
    live.sort_by_key(Lease::id);
    assert_eq!(
        store.retire(generation, None).unwrap(),
        Retirement::Draining(live.clone())
    );
    assert_eq!(store.leases(generation).unwrap(), live);
    for sandbox in &sandboxes[2..] {
        assert!(release(&store, sandbox).unwrap().is_some());
    }
    assert_eq!(store.retire(generation, None).unwrap(), Retirement::Retired);
}

#[test]
fn interrupted_retirements_finish() {
    let (directory, store) = new_store();
    let first = publish(&store, &ManifestSpec::template(), storage(1));
    let second = publish(&store, &ManifestSpec::template(), storage(2));
    let third = publish(&store, &ManifestSpec::template(), storage(3));
    // One that was interrupted once it had removed the payload leaves the generation indexed and
    // quarantined.
    store.tombstone(first.generation(), None).unwrap();
    fs::remove_dir_all(store.payload_dir(first.generation())).unwrap();
    assert_eq!(
        store.retire(first.generation(), None).unwrap(),
        Retirement::Retired
    );
    assert_eq!(store.index().unwrap().get(first.generation()), None);
    // One that was interrupted once it had removed the entry from the index leaves the tombstone.
    store.tombstone(second.generation(), None).unwrap();
    fs::remove_dir_all(store.payload_dir(second.generation())).unwrap();
    remove(&store, second.generation());
    assert_eq!(store.tombstones().unwrap(), [second.generation()]);
    assert_eq!(
        store.retire(second.generation(), None).unwrap(),
        Retirement::Retired
    );
    assert!(store.tombstones().unwrap().is_empty());
    // Interrupted replacements of records leave temporary files, which listings skip, and which
    // a retirement removes under the store's lock.
    let sandbox = provision(directory.path(), "sandbox");
    let lease = leased(&store, &third, &sandbox);
    store.tombstone(third.generation(), None).unwrap();
    let temporaries = [
        store
            .dir()
            .join(LEASES_NAME)
            .join(format!(".{}.json.42.tmp", lease.id())),
        store.dir().join(TOMBSTONES_NAME).join(".7.json.1.tmp"),
    ];
    for temporary in &temporaries {
        fs::write(temporary, b"{").unwrap();
    }
    assert_eq!(
        store.leases(third.generation()).unwrap(),
        std::slice::from_ref(&lease)
    );
    assert_eq!(store.tombstones().unwrap(), [third.generation()]);
    assert_eq!(
        store.retire(third.generation(), None).unwrap(),
        Retirement::Draining(vec![lease])
    );
    for temporary in &temporaries {
        assert!(!temporary.exists(), "{}", temporary.display());
    }
}

#[test]
fn acquisitions_make_their_records_durable_before_their_bindings_and_both_before_they_return() {
    let (directory, store) = new_store();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let leases_dir = store.dir().join(LEASES_NAME);
    let first = provision(directory.path(), "first");
    let second = provision(directory.path(), "second");
    // Each sync notes its directory, the records of leases in the store, and the bindings of the
    // sandboxes.
    let synced = RefCell::new(Vec::new());
    let sync = |dir: &Path| -> Result<()> {
        let records = fs::read_dir(&leases_dir).map_or(0, Iterator::count);
        let bindings = [&first, &second]
            .into_iter()
            .filter(|sandbox| sandbox.join(CLONE_BINDING_NAME).exists())
            .count();
        synced
            .borrow_mut()
            .push((dir.to_owned(), records, bindings));
        Ok(())
    };
    // An acquisition that creates the directory of the leases, but cannot make its entry in the
    // store durable, leases nothing, and the next acquisition syncs the store's directory again.
    let now = after(&published, 1);
    let unsynced_store = |dir: &Path| {
        if dir == store.dir() {
            Err(Error::io("the disk failed"))
        } else {
            Ok(())
        }
    };
    let error = store
        .acquire_with(
            published.generation(),
            first.clone(),
            &files(),
            &now,
            None,
            &unsynced_store,
        )
        .unwrap_err();
    assert_eq!(error.message(), "the disk failed");
    assert!(leases_dir.is_dir());
    assert_eq!(binding(&first), None);
    for sandbox in [&first, &second] {
        let acquired = store
            .acquire_with(
                published.generation(),
                sandbox.clone(),
                &files(),
                &now,
                None,
                &sync,
            )
            .unwrap();
        drop(acquired);
    }
    assert_eq!(
        *synced.borrow(),
        [
            // The first makes the entry of the directory of the leases durable, which no
            // acquisition of this process did before, ahead of any record in it, and then the
            // store's own entry.
            (store.dir().to_owned(), 0, 0),
            (store.dir().parent().unwrap().to_owned(), 0, 0),
            // Each makes its record durable before its binding, and its binding before it
            // returns.
            (leases_dir.clone(), 1, 0),
            (first.clone(), 1, 1),
            (leases_dir.clone(), 2, 1),
            (second.clone(), 2, 2),
        ]
    );
}

#[test]
fn an_acquisition_keeps_its_record_while_its_binding_may_outlast_a_crash() {
    let (directory, store) = new_store();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let generation = published.generation();
    let now = after(&published, 1);
    let sandbox = provision(directory.path(), "sandbox");
    let leases_dir = store.dir().join(LEASES_NAME);
    let failed = || Err(Error::io("the disk failed"));
    let acquire = |sync: SyncDir<'_>| {
        store
            .acquire_with(generation, sandbox.clone(), &files(), &now, None, sync)
            .unwrap_err()
    };
    // A record that cannot be made durable goes before any binding names it.
    let error = acquire(&|dir: &Path| if dir == leases_dir { failed() } else { Ok(()) });
    assert_eq!(error.message(), "the disk failed");
    assert_eq!(binding(&sandbox), None);
    assert!(store.leases(generation).unwrap().is_empty());
    // A binding that cannot be made durable goes, and its record once the binding's removal is
    // durable.
    let first = Cell::new(true);
    let error = acquire(&|dir: &Path| {
        if dir == sandbox && first.replace(false) {
            failed()
        } else {
            Ok(())
        }
    });
    assert_eq!(error.message(), "the disk failed");
    assert_eq!(binding(&sandbox), None);
    assert!(store.leases(generation).unwrap().is_empty());
    // A binding whose removal cannot be made durable either may outlast a crash, so its record
    // stays, and keeps the generation, until a retirement finds that no binding names it.
    let error = acquire(&|dir: &Path| if dir == sandbox { failed() } else { Ok(()) });
    let [kept]: [Lease; 1] = store.leases(generation).unwrap().try_into().unwrap();
    assert_eq!(
        error.message(),
        format!(
            "the disk failed; the sandbox in {} may hold the lease {} still, which a release ends",
            sandbox.display(),
            kept.id()
        )
    );
    assert_eq!(binding(&sandbox), None);
    assert_eq!(store.retire(generation, None).unwrap(), Retirement::Retired);
}

#[test]
fn a_release_makes_the_removal_of_its_binding_durable_before_it_removes_its_record() {
    let (directory, store) = new_store();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let generation = published.generation();
    let sandbox = provision(directory.path(), "sandbox");
    let lease = leased(&store, &published, &sandbox);
    let record = store
        .dir()
        .join(LEASES_NAME)
        .join(format!("{}.json", lease.id()));
    let payload = store.payload_dir(generation);
    let socket = store.clone_socket_path(&lease);
    // Each sync notes its directory, and whether any file that the clone kept of its launch, the
    // control socket of its VMM and the scratch file that an earlier version left for a clone in
    // RAM included, the binding, and the record existed then.
    let kept = [
        CLONE_SCRATCH_NAME,
        CLONE_ACTIVATION_NAME,
        CLONE_PROGRESS_NAME,
        CLONE_FAILURE_NAME,
        RESTORE_READY_NAME,
    ];
    let synced = RefCell::new(Vec::new());
    let note = |dir: &Path| {
        let clone = kept
            .iter()
            .any(|name| fs::symlink_metadata(sandbox.join(name)).is_ok())
            || fs::symlink_metadata(&socket).is_ok();
        let bound = sandbox.join(CLONE_BINDING_NAME).exists();
        synced
            .borrow_mut()
            .push((dir.to_owned(), clone, bound, record.exists()));
    };
    // The release syncs the payload through its handle, which holds the generation's snapshot.
    let snapshot = platform::file_id(&payload.join(SNAPSHOT_NAME)).unwrap();
    let sync_payload = |directory: &platform::Directory| -> io::Result<()> {
        assert_eq!(directory.entry_id(SNAPSHOT_NAME).unwrap(), Some(snapshot));
        note(&payload);
        Ok(())
    };
    // The release syncs the sandbox's directory through its handle, which holds the lock's file.
    let lock = platform::file_id(&sandbox.join(LOCK_NAME)).unwrap();
    let sync_sandbox = |directory: &platform::Directory| -> io::Result<()> {
        assert_eq!(directory.entry_id(LOCK_NAME).unwrap(), Some(lock));
        note(&sandbox);
        Ok(())
    };
    for name in kept {
        fs::write(sandbox.join(name), b"").unwrap();
    }
    // A socket that a VMM on Linux left in the payload, which a file stands for.
    if cfg!(unix) {
        fs::write(&socket, b"").unwrap();
    }
    let mut locked = SandboxStore::lock_existing(&sandbox, &files(), None).unwrap();
    assert_eq!(
        store
            .release_with(&mut locked, &|| {}, &sync_payload, &sync_sandbox)
            .unwrap(),
        Some(lease)
    );
    drop(locked);
    let mut expected = vec![(sandbox.clone(), false, false, true)];
    if cfg!(unix) {
        expected.insert(0, (payload.clone(), false, true, true));
    }
    assert_eq!(*synced.borrow(), expected);
    assert!(!record.exists());
    assert!(fs::symlink_metadata(&socket).is_err());
    // A release under the lock that acquired the lease ends the lock's reservation before it
    // removes the clone's files, such as the record of its failure, which ends the clone's
    // launches until the release, so that one that fails after their removal leaves no launch to
    // begin.
    let Acquired {
        lease: again,
        mut locked,
    } = store
        .acquire(
            generation,
            sandbox.as_path(),
            &files(),
            &after(&published, 1),
            None,
        )
        .unwrap();
    assert_eq!(locked.reserved(), Some(again.launch()));
    let failure = sandbox.join(CLONE_FAILURE_NAME);
    fs::write(&failure, b"").unwrap();
    // A release that cannot make the socket's removal durable keeps the binding, and the lease.
    if cfg!(unix) {
        let error = store
            .release_with(
                &mut locked,
                &|| {},
                &|_| Err(io::Error::other("the disk failed")),
                &|_| Ok(()),
            )
            .unwrap_err();
        assert_eq!(
            error.message(),
            format!("cannot sync the directory {}", payload.display())
        );
        assert_eq!(cause(&error), "the disk failed");
        assert_eq!(binding(&sandbox), Some(again.clone()));
        assert_eq!(locked.reserved(), None);
        assert!(fs::symlink_metadata(&failure).is_err());
    }
    // A release that cannot make the binding's removal durable keeps the record, which a
    // retirement removes once no binding names it.
    let error = store
        .release_with(&mut locked, &|| {}, &|_| Ok(()), &|_| {
            Err(io::Error::other("the disk failed"))
        })
        .unwrap_err();
    assert_eq!(locked.reserved(), None);
    assert!(fs::symlink_metadata(&failure).is_err());
    drop(locked);
    assert_eq!(
        error.message(),
        format!("cannot sync the directory {}", sandbox.display())
    );
    assert_eq!(cause(&error), "the disk failed");
    assert_eq!(binding(&sandbox), None);
    assert_eq!(store.leases(generation).unwrap(), [again]);
    assert_eq!(store.retire(generation, None).unwrap(), Retirement::Retired);
}

#[test]
fn a_retirement_makes_each_step_durable_before_the_next() {
    let (_directory, store) = new_store();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let generation = published.generation();
    let tombstones = store.dir().join(TOMBSTONES_NAME);
    let tombstone = tombstones.join(format!("{generation}.json"));
    let payload = store.payload_dir(generation);
    // Each sync notes its directory, and whether the tombstone, the payload, and the
    // generation's entry in the index existed then.
    let synced = RefCell::new(Vec::new());
    let sync = |dir: &Path| -> Result<()> {
        let indexed = store.index()?.get(generation).is_some();
        synced.borrow_mut().push((
            dir.to_owned(),
            tombstone.exists(),
            payload.exists(),
            indexed,
        ));
        Ok(())
    };
    assert_eq!(
        store.retire_with(generation, None, &sync).unwrap(),
        Retirement::Retired
    );
    assert_eq!(
        *synced.borrow(),
        [
            // The new directory of the tombstones, and the store's own entry, before the
            // tombstone in it.
            (store.dir().to_owned(), false, true, true),
            (store.dir().parent().unwrap().to_owned(), false, true, true),
            // The tombstone, before anything goes.
            (tombstones.clone(), true, true, true),
            // The payload's removal, before the index changes.
            (store.dir().join(GENERATIONS_NAME), true, false, true),
            // The index, before the tombstone goes.
            (store.dir().to_owned(), true, false, false),
        ]
    );
    assert!(!tombstone.exists());
}

#[test]
fn a_retirement_makes_the_absence_of_a_binding_durable_before_it_removes_its_record() {
    let (directory, store) = new_store();
    let root = filesystem::canonicalize(directory.path()).unwrap();
    let first = publish(&store, &ManifestSpec::template(), storage(1));
    let second = publish(&store, &ManifestSpec::template(), storage(2));
    let third = publish(&store, &ManifestSpec::template(), storage(3));
    let fourth = publish(&store, &ManifestSpec::template(), storage(4));
    let released = provision(&root, "released");
    let removed = provision(&root, "removed");
    let reprovisioned = provision(&root, "reprovisioned");
    let sandboxes = root.join("sandboxes");
    fs::create_dir(&sandboxes).unwrap();
    let removed_through_link = provision(&sandboxes, "removed");
    let released_lease = leased(&store, &first, &released);
    let removed_lease = leased(&store, &second, &removed);
    let linked_lease = leased(&store, &third, &removed_through_link);
    let reprovisioned_lease = leased(&store, &fourth, &reprovisioned);
    // The directory that holds a sandbox moves, and a link at its path names it, so the path that
    // names the sandbox's lease runs through the link.
    let moved = root.join("moved");
    fs::rename(&sandboxes, &moved).unwrap();
    link_dir(&moved, &sandboxes);
    // A release that removed its binding, but did not make that durable, and removals of
    // sandboxes, one of which is provisioned again at its path.
    fs::remove_file(released.join(CLONE_BINDING_NAME)).unwrap();
    for sandbox in [&removed, &removed_through_link, &reprovisioned] {
        let locked = SandboxStore::lock_existing(sandbox, &files(), None).unwrap();
        assert!(locked.remove().unwrap().is_none());
    }
    assert_eq!(provision(&root, "reprovisioned"), reprovisioned);
    let record = |lease: &Lease| {
        store
            .dir()
            .join(LEASES_NAME)
            .join(format!("{}.json", lease.id()))
    };
    // A retirement that cannot make the binding's absence durable keeps its record, and the
    // generation.
    let unsynced_sandbox = |dir: &Path| {
        if dir == released {
            Err(Error::io("the disk failed"))
        } else {
            Ok(())
        }
    };
    let error = store
        .retire_with(first.generation(), None, &unsynced_sandbox)
        .unwrap_err();
    assert_eq!(error.message(), "the disk failed");
    assert!(record(&released_lease).exists());
    assert!(store.snapshot_dir(&first).is_ok());
    // A record goes once the binding's absence is durable: in the sandbox's directory, if it is
    // one, and in the directory that holds its entry, the nearest of its ancestors that remains,
    // through the links that its path names. The directory of a sandbox that was provisioned
    // again never held the binding, which went with the directory that its entry named before.
    for (generation, lease, dirs) in [
        (
            &first,
            &released_lease,
            vec![released.as_path(), root.as_path()],
        ),
        (&second, &removed_lease, vec![root.as_path()]),
        (&third, &linked_lease, vec![sandboxes.as_path()]),
        (
            &fourth,
            &reprovisioned_lease,
            vec![reprovisioned.as_path(), root.as_path()],
        ),
    ] {
        let path = record(lease);
        let synced = RefCell::new(Vec::new());
        let sync = |synced_dir: &Path| -> Result<()> {
            synced
                .borrow_mut()
                .push((synced_dir.to_owned(), path.exists()));
            Ok(())
        };
        assert_eq!(
            store
                .retire_with(generation.generation(), None, &sync)
                .unwrap(),
            Retirement::Retired
        );
        let mut expected = vec![(store.dir().join(TOMBSTONES_NAME), true)];
        expected.extend(dirs.into_iter().map(|dir| (dir.to_owned(), true)));
        expected.extend([
            (store.dir().join(GENERATIONS_NAME), false),
            (store.dir().to_owned(), false),
        ]);
        assert_eq!(*synced.borrow(), expected);
    }
    // A link to nothing hides the directory that held a sandbox, so the record stays.
    let fifth = publish(&store, &ManifestSpec::template(), storage(5));
    let parked = root.join("parked");
    fs::create_dir(&parked).unwrap();
    let dangling = provision(&parked, "dangling");
    let dangling_lease = leased(&store, &fifth, &dangling);
    let locked = SandboxStore::lock_existing(&dangling, &files(), None).unwrap();
    assert!(locked.remove().unwrap().is_none());
    let away = root.join("away");
    fs::rename(&parked, &away).unwrap();
    link_dir(&away, &parked);
    fs::remove_dir(&away).unwrap();
    let error = store.retire(fifth.generation(), None).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(
        error.message().contains("cannot find the directory"),
        "{error}"
    );
    assert!(record(&dangling_lease).exists());
    assert!(store.snapshot_dir(&fifth).is_ok());
}

#[test]
fn a_lease_names_its_sandbox_by_its_path_with_its_links_resolved() {
    let (directory, store) = new_store();
    let root = filesystem::canonicalize(directory.path()).unwrap();
    let first = publish(&store, &ManifestSpec::template(), storage(1));
    let second = publish(&store, &ManifestSpec::template(), storage(2));
    let sandboxes = root.join("sandboxes");
    fs::create_dir(&sandboxes).unwrap();
    let parent = provision(&sandboxes, "parent");
    let linked = provision(&sandboxes, "linked");
    // Paths through a directory that a `..` component passes through, and through a link, both of
    // which go while the clones run.
    let helper = root.join("helper");
    fs::create_dir(&helper).unwrap();
    let link = root.join("link");
    link_dir(&sandboxes, &link);
    let mut leases = Vec::new();
    for (generation, sandbox, path) in [
        (
            &first,
            &parent,
            helper.join("..").join("sandboxes").join("parent"),
        ),
        (&second, &linked, link.join("linked")),
    ] {
        let acquired = store
            .acquire(
                generation.generation(),
                &path,
                &files(),
                &after(generation, 1),
                None,
            )
            .unwrap();
        assert_eq!(acquired.lease.sandbox(), sandbox);
        assert_eq!(acquired.locked.dir(), sandbox);
        drop(acquired.locked);
        assert_eq!(binding(sandbox), Some(acquired.lease.clone()));
        leases.push(acquired.lease);
    }
    fs::remove_dir(&helper).unwrap();
    unlink_dir(&link);
    // A retirement still finds both bindings, and keeps the generations.
    for (generation, lease) in [&first, &second].into_iter().zip(leases) {
        assert_eq!(
            store.retire(generation.generation(), None).unwrap(),
            Retirement::Draining(vec![lease])
        );
    }
}

#[test]
fn an_acquisition_binds_the_sandbox_that_it_resolved_whatever_its_path_names_meanwhile() {
    let (directory, store) = new_store();
    let root = filesystem::canonicalize(directory.path()).unwrap();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let (first, second) = (root.join("first"), root.join("second"));
    fs::create_dir(&first).unwrap();
    fs::create_dir(&second).unwrap();
    let leased_sandbox = provision(&first, "sandbox");
    let other = provision(&second, "sandbox");
    let link = root.join("link");
    link_dir(&first, &link);
    // The link names the other sandbox's directory once the acquisition has resolved its path, as
    // it makes the lease's record durable.
    let retargeted = Cell::new(false);
    let retarget = |dir: &Path| -> Result<()> {
        if dir == store.dir().join(LEASES_NAME) && !retargeted.replace(true) {
            unlink_dir(&link);
            link_dir(&second, &link);
        }
        Ok(())
    };
    let acquired = store
        .acquire_with(
            published.generation(),
            link.join("sandbox"),
            &files(),
            &after(&published, 1),
            None,
            &retarget,
        )
        .unwrap();
    assert!(retargeted.get());
    // The binding, and the lock, are those of the sandbox that the lease names.
    assert_eq!(acquired.lease.sandbox(), leased_sandbox);
    assert_eq!(acquired.locked.dir(), leased_sandbox);
    drop(acquired.locked);
    assert_eq!(binding(&leased_sandbox), Some(acquired.lease.clone()));
    assert_eq!(binding(&other), None);
    assert_eq!(
        store.retire(published.generation(), None).unwrap(),
        Retirement::Draining(vec![acquired.lease])
    );
}

#[test]
fn a_release_takes_the_lock_of_its_sandbox_through_the_path_of_its_lease() {
    let (directory, store) = new_store();
    let root = filesystem::canonicalize(directory.path()).unwrap();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let generation = published.generation();
    let (first, second) = (root.join("first"), root.join("second"));
    fs::create_dir(&first).unwrap();
    fs::create_dir(&second).unwrap();
    let leased_sandbox = provision(&first, "sandbox");
    let other = provision(&second, "sandbox");
    let lease = leased(&store, &published, &leased_sandbox);
    let other_lease = leased(&store, &published, &other);
    let mut both = vec![lease.clone(), other_lease.clone()];
    both.sort_by_key(Lease::id);
    let link = root.join("link");
    link_dir(&first, &link);
    // A lock taken through a link refuses a release, which removes nothing, also once the link
    // names another sandbox, whose lease a release would otherwise end without its lock: the
    // directory that the link names then holds another lock file than the one locked.
    let mut locked = SandboxStore::lock_existing(link.join("sandbox"), &files(), None).unwrap();
    for (retargeted, refusal) in [
        (false, "does not name it by the path of its lease"),
        (true, "does not hold the lock of its sandbox"),
    ] {
        if retargeted {
            unlink_dir(&link);
            link_dir(&second, &link);
        }
        let error = store.release(&mut locked).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Other, "{error}");
        assert!(error.message().contains(refusal), "{error}");
        assert_eq!(binding(&leased_sandbox), Some(lease.clone()));
        assert_eq!(binding(&other), Some(other_lease.clone()));
        assert_eq!(store.leases(generation).unwrap(), both);
    }
    drop(locked);
    // A lock taken through the lease's path ends it.
    assert_eq!(release(&store, lease.sandbox()).unwrap(), Some(lease));
    assert_eq!(binding(&leased_sandbox), None);
    assert_eq!(store.leases(generation).unwrap(), [other_lease]);
}

/// A release whose path comes to name another sandbox, once a directory in the path is renamed,
/// and a link to the other sandbox's parent takes its place: only on Unix, since Windows refuses
/// to rename a directory that holds an open file, such as the lock's.
#[cfg(unix)]
#[test]
fn a_release_ends_the_lease_of_the_sandbox_whose_lock_it_holds_whatever_its_path_names_meanwhile() {
    let (directory, store) = new_store();
    let root = filesystem::canonicalize(directory.path()).unwrap();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let generation = published.generation();
    let (first, second) = (root.join("first"), root.join("second"));
    fs::create_dir(&first).unwrap();
    fs::create_dir(&second).unwrap();
    let leased_sandbox = provision(&first, "sandbox");
    let other = provision(&second, "sandbox");
    let lease = leased(&store, &published, &leased_sandbox);
    let other_lease = leased(&store, &published, &other);
    let kept = [
        CLONE_SCRATCH_NAME,
        CLONE_ACTIVATION_NAME,
        CLONE_PROGRESS_NAME,
        CLONE_FAILURE_NAME,
        RESTORE_READY_NAME,
    ];
    for sandbox in [&leased_sandbox, &other] {
        for name in kept {
            fs::write(sandbox.join(name), b"").unwrap();
        }
    }
    // Moves `parent` to `to`, and links `parent` to the other sandbox's parent.
    let swap = |parent: &Path, to: &Path| {
        fs::rename(parent, to).unwrap();
        link_dir(&second, parent);
    };
    // The other sandbox's files, and its lease, stay.
    let other_stays = || {
        for name in kept {
            assert!(other.join(name).exists(), "{name}");
        }
        assert_eq!(binding(&other), Some(other_lease.clone()));
    };
    // Once the release has checked the sandbox, its path comes to name the other sandbox, from
    // the removal of its files on.
    let moved = root.join("moved");
    let mut locked = SandboxStore::lock_existing(&leased_sandbox, &files(), None).unwrap();
    assert_eq!(
        store
            .release_with(
                &mut locked,
                &|| swap(&first, &moved),
                &platform::Directory::sync,
                &platform::Directory::sync
            )
            .unwrap(),
        Some(lease)
    );
    drop(locked);
    // The release removed the files and the binding of the sandbox whose lock it held.
    let released = moved.join("sandbox");
    for name in kept {
        assert!(fs::symlink_metadata(released.join(name)).is_err(), "{name}");
    }
    assert_eq!(binding(&released), None);
    other_stays();
    assert_eq!(
        store.leases(generation).unwrap(),
        std::slice::from_ref(&other_lease)
    );
    // A release whose path names the other sandbox once the lock is taken, before the release
    // begins, refuses, and removes nothing.
    let again = leased(&store, &published, &released);
    for name in kept {
        fs::write(released.join(name), b"").unwrap();
    }
    let mut locked = SandboxStore::lock_existing(&released, &files(), None).unwrap();
    let away = root.join("away");
    swap(&moved, &away);
    let error = store.release(&mut locked).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Other, "{error}");
    assert!(
        error
            .message()
            .contains("does not hold the lock of its sandbox"),
        "{error}"
    );
    drop(locked);
    let kept_sandbox = away.join("sandbox");
    for name in kept {
        assert!(kept_sandbox.join(name).exists(), "{name}");
    }
    assert_eq!(binding(&kept_sandbox), Some(again.clone()));
    other_stays();
    let mut both = vec![again, other_lease];
    both.sort_by_key(Lease::id);
    assert_eq!(store.leases(generation).unwrap(), both);
}

/// The payload of a generation, which holds the control sockets of the VMMs of its clones on
/// Linux, opens through the store's directory, following no link, or junction on Windows, at the
/// store's directory, at its directory of the generations, or at the payload, once it has found
/// that the store's directory holds the store of the caller's identity. On Unix, a store's
/// directory and its directory of the generations need only the right to be searched, which an
/// owner-only directory may grant without the right to read it.
#[test]
fn the_payload_of_a_generation_opens_through_no_link() {
    let (directory, store) = new_store();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let generation = published.generation();
    let id = store.id();
    let payload = store.payload_dir(generation);
    let snapshot = platform::file_id(&payload.join(SNAPSHOT_NAME)).unwrap();
    let opened = open_payload(store.dir(), id, generation).unwrap().unwrap();
    assert_eq!(opened.entry_id(SNAPSHOT_NAME).unwrap(), Some(snapshot));
    drop(opened);
    // A generation without a payload has none, nor has a store that is gone, or one of another
    // identity.
    assert!(
        open_payload(store.dir(), id, store_generation(9))
            .unwrap()
            .is_none()
    );
    let missing = directory.path().join("missing");
    assert!(open_payload(&missing, id, generation).unwrap().is_none());
    let foreign = StoreId::from_bytes([0x5a; 16]);
    assert!(
        open_payload(store.dir(), foreign, generation)
            .unwrap()
            .is_none()
    );
    // Another store in the store's place, whose generation of the same number has a payload, has
    // none of the store's, nor has a directory that holds no store.
    let other = Store::create(directory.path().join("other")).unwrap();
    fs::create_dir(other.payload_dir(generation)).unwrap();
    let moved = directory.path().join("moved");
    fs::rename(store.dir(), &moved).unwrap();
    fs::rename(other.dir(), store.dir()).unwrap();
    assert!(open_payload(store.dir(), id, generation).unwrap().is_none());
    assert!(
        open_payload(store.dir(), other.id(), generation)
            .unwrap()
            .is_some()
    );
    fs::rename(store.dir(), other.dir()).unwrap();
    fs::create_dir(store.dir()).unwrap();
    assert!(open_payload(store.dir(), id, generation).unwrap().is_none());
    fs::remove_dir(store.dir()).unwrap();
    // A link in the place of the store's directory, even to the store, of the directory of the
    // generations, or of the payload, and a file in the place of the store's directory or of the
    // payload, are refused.
    let generations = store.dir().join(GENERATIONS_NAME);
    let refused = |entry: &dyn std::fmt::Display| {
        let error = open_payload(store.dir(), id, generation).err().unwrap();
        assert_eq!(error.kind(), ErrorKind::Io, "{error}");
        assert_eq!(
            error.message(),
            format!(
                "cannot open the payload of the generation {}",
                payload.display()
            )
        );
        assert_eq!(
            cause(&error),
            format!("{entry} is a link, or not a directory")
        );
    };
    link_dir(&moved, store.dir());
    refused(&store.dir().display());
    unlink_dir(store.dir());
    fs::write(store.dir(), b"").unwrap();
    refused(&store.dir().display());
    fs::remove_file(store.dir()).unwrap();
    fs::rename(&moved, store.dir()).unwrap();
    for (path, name, link) in [
        (&payload, generation.to_string(), true),
        (&generations, GENERATIONS_NAME.to_owned(), true),
        (&payload, generation.to_string(), false),
    ] {
        fs::rename(path, &moved).unwrap();
        if link {
            link_dir(&moved, path);
        } else {
            fs::write(path, b"").unwrap();
        }
        refused(&name);
        if link {
            unlink_dir(path);
        } else {
            fs::remove_file(path).unwrap();
        }
        fs::rename(&moved, path).unwrap();
    }
    assert!(open_payload(store.dir(), id, generation).unwrap().is_some());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = |dir: &Path, mode| {
            fs::set_permissions(dir, fs::Permissions::from_mode(mode)).unwrap();
        };
        for dir in [&generations, store.dir()] {
            mode(dir, 0o300);
        }
        let opened = open_payload(store.dir(), id, generation);
        for dir in [&generations, store.dir()] {
            mode(dir, 0o700);
        }
        let opened = opened.unwrap().unwrap();
        assert_eq!(opened.entry_id(SNAPSHOT_NAME).unwrap(), Some(snapshot));
        opened.sync().unwrap();
    }
}

/// A release whose generation's payload, or store's directory, a link takes the place of: before
/// the release begins, it refuses, and removes nothing; once it has opened the payload, it removes
/// the socket from the payload that it opened, and the link's target keeps its file. Only on Unix,
/// where the payload holds the socket of the clone's VMM.
#[cfg(unix)]
#[test]
fn a_release_removes_the_socket_from_the_payload_that_it_opened_whatever_its_path_names_meanwhile()
{
    let (directory, store) = new_store();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let generation = published.generation();
    let sandbox = provision(directory.path(), "sandbox");
    let lease = leased(&store, &published, &sandbox);
    let payload = store.payload_dir(generation);
    let name = clone_socket_name(lease.launch());
    // A socket that the VMM left in the payload, which a file stands for, and a directory that
    // holds a file of its name, to which a link takes the payload's place.
    fs::write(payload.join(&name), b"").unwrap();
    let decoy = directory.path().join("decoy");
    fs::create_dir(&decoy).unwrap();
    fs::write(decoy.join(&name), b"").unwrap();
    // A directory of the store's layout, whose generation of the same number holds a file of the
    // socket's name, to which a link takes the store's place.
    let decoy_store = directory.path().join("decoy-store");
    let decoy_socket = decoy_store
        .join(GENERATIONS_NAME)
        .join(generation.to_string())
        .join(&name);
    fs::create_dir_all(decoy_socket.parent().unwrap()).unwrap();
    fs::write(&decoy_socket, b"").unwrap();
    let moved = directory.path().join("moved");
    let swap = || {
        fs::rename(&payload, &moved).unwrap();
        link_dir(&decoy, &payload);
    };
    let restore = || {
        unlink_dir(&payload);
        fs::rename(&moved, &payload).unwrap();
    };
    let mut locked = SandboxStore::lock_existing(&sandbox, &files(), None).unwrap();
    let refused = |locked: &mut LockedStore, entry: &dyn std::fmt::Display| {
        let error = store.release(locked).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{error}");
        assert_eq!(
            error.message(),
            format!(
                "cannot open the payload of the generation {}",
                payload.display()
            )
        );
        assert_eq!(
            cause(&error),
            format!("{entry} is a link, or not a directory")
        );
        assert!(decoy.join(&name).exists());
        assert!(decoy_socket.exists());
        assert_eq!(binding(&sandbox), Some(lease.clone()));
    };
    swap();
    refused(&mut locked, &generation);
    assert!(moved.join(&name).exists());
    restore();
    fs::rename(store.dir(), &moved).unwrap();
    link_dir(&decoy_store, store.dir());
    refused(&mut locked, &store.dir().display());
    unlink_dir(store.dir());
    fs::rename(&moved, store.dir()).unwrap();
    assert!(payload.join(&name).exists());
    assert_eq!(
        store
            .release_with(
                &mut locked,
                &swap,
                &platform::Directory::sync,
                &platform::Directory::sync
            )
            .unwrap(),
        Some(lease)
    );
    drop(locked);
    assert!(fs::symlink_metadata(moved.join(&name)).is_err());
    assert!(decoy.join(&name).exists());
    assert!(decoy_socket.exists());
    assert_eq!(binding(&sandbox), None);
    restore();
    assert!(store.leases(generation).unwrap().is_empty());
}

/// A sandbox's store that clears the runtime record of its clone's VM removes the VMM's control
/// socket through the payload of its generation, as a recovery of its launch does: it refuses a
/// link in the place of the payload or of the template store's directory, keeping the record, and
/// leaves the files of another store in the template store's place. Only on Unix, where the socket
/// is a file.
#[cfg(unix)]
#[test]
fn a_sandbox_s_store_removes_the_socket_of_its_clone_through_no_link() {
    let (directory, store) = new_store();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let generation = published.generation();
    let sandbox = provision(directory.path(), "sandbox");
    let lease = leased(&store, &published, &sandbox);
    let payload = store.payload_dir(generation);
    let socket = store.clone_socket_path(&lease);
    let name = clone_socket_name(lease.launch());
    fs::write(&socket, b"").unwrap();
    let decoy = directory.path().join("decoy");
    fs::create_dir(&decoy).unwrap();
    fs::write(decoy.join(&name), b"").unwrap();
    // Another store, whose generation of the same number holds a file of the socket's name.
    let other = Store::create(directory.path().join("other")).unwrap();
    fs::create_dir(other.payload_dir(generation)).unwrap();
    let other_socket = other.payload_dir(generation).join(&name);
    fs::write(&other_socket, b"").unwrap();
    // The runtime record of the clone's VM, whose control endpoint is the socket.
    let record = json!({
        "format": FORMAT,
        "launch": lease.launch().to_string(),
        "process": { "pid": 42, "startTime": 4242, "boot": "ab".repeat(16) },
        "endpoint": socket.to_str().unwrap(),
        "runtime": serde_json::to_value(runtime()).unwrap(),
    });
    let write_record = || {
        write_private(&sandbox.join(RUNTIME_NAME), record.to_string().as_bytes()).unwrap();
    };
    write_record();
    let mut locked = SandboxStore::lock_existing(&sandbox, &files(), None).unwrap();
    let refused = |locked: &mut LockedStore, entry: &dyn std::fmt::Display| {
        let error = locked.clear_runtime().unwrap_err();
        assert_eq!(
            error.message(),
            format!(
                "cannot open the payload of the generation {}",
                payload.display()
            )
        );
        assert_eq!(
            cause(&error),
            format!("{entry} is a link, or not a directory")
        );
        assert!(decoy.join(&name).exists());
        assert!(other_socket.exists());
        assert!(sandbox.join(RUNTIME_NAME).exists());
    };
    let moved = directory.path().join("moved");
    fs::rename(&payload, &moved).unwrap();
    link_dir(&decoy, &payload);
    refused(&mut locked, &generation);
    assert!(moved.join(&name).exists());
    unlink_dir(&payload);
    fs::rename(&moved, &payload).unwrap();
    fs::rename(store.dir(), &moved).unwrap();
    link_dir(other.dir(), store.dir());
    refused(&mut locked, &store.dir().display());
    unlink_dir(store.dir());
    // Another store in the store's place holds no socket of the clone's, so the record goes, and
    // the other store's file stays, as does the socket in the store's own directory.
    fs::rename(other.dir(), store.dir()).unwrap();
    locked.clear_runtime().unwrap();
    assert!(!sandbox.join(RUNTIME_NAME).exists());
    fs::rename(store.dir(), other.dir()).unwrap();
    fs::rename(&moved, store.dir()).unwrap();
    assert!(other_socket.exists());
    assert!(socket.exists());
    write_record();
    locked.clear_runtime().unwrap();
    assert!(fs::symlink_metadata(&socket).is_err());
    assert!(!sandbox.join(RUNTIME_NAME).exists());
    assert!(decoy.join(&name).exists());
    assert!(other_socket.exists());
    drop(locked);
    assert_eq!(release(&store, &sandbox).unwrap(), Some(lease));
}

#[test]
fn a_retirement_that_finds_its_generation_unindexed_makes_the_index_durable_first() {
    let (_directory, store) = new_store();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let generation = published.generation();
    // A retirement that a crash interrupted once it had written the index, which it may not have
    // made durable, leaves the tombstone.
    store.tombstone(generation, None).unwrap();
    fs::remove_dir_all(store.payload_dir(generation)).unwrap();
    remove(&store, generation);
    let tombstone = store
        .dir()
        .join(TOMBSTONES_NAME)
        .join(format!("{generation}.json"));
    let synced = RefCell::new(Vec::new());
    let sync = |dir: &Path| -> Result<()> {
        synced
            .borrow_mut()
            .push((dir.to_owned(), tombstone.exists()));
        Ok(())
    };
    assert_eq!(
        store.retire_with(generation, None, &sync).unwrap(),
        Retirement::Retired
    );
    assert_eq!(*synced.borrow(), [(store.dir().to_owned(), true)]);
    assert!(!tombstone.exists());
}

#[test]
fn a_retirement_keeps_a_tombstone_that_is_not_the_stores() {
    let (directory, store) = new_store();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let generation = published.generation();
    // A retirement that a crash interrupted once it had written the index leaves the tombstone,
    // which retiring the generation again removes only once it has checked it.
    store.tombstone(generation, None).unwrap();
    fs::remove_dir_all(store.payload_dir(generation)).unwrap();
    remove(&store, generation);
    let tombstone = store
        .dir()
        .join(TOMBSTONES_NAME)
        .join(format!("{generation}.json"));
    let valid = json_of(&tombstone);
    let foreign: [(Change, ErrorCode); 2] = [
        (
            |value| value["store"] = json!("0".repeat(32)),
            ErrorCode::Io,
        ),
        (
            |value| value["format"] = json!(2),
            ErrorCode::FormatUnsupported,
        ),
    ];
    for (change, code) in foreign {
        let mut value = valid.clone();
        change(&mut value);
        fs::write(&tombstone, value.to_string()).unwrap();
        let error = store.retire(generation, None).unwrap_err();
        assert_eq!(error.code(), code, "{error}");
        assert_eq!(json_of(&tombstone), value);
    }
    // Nor does it remove a link in place of the tombstone.
    fs::write(&tombstone, valid.to_string()).unwrap();
    let moved = directory.path().join("tombstone.json");
    fs::rename(&tombstone, &moved).unwrap();
    if link_file(&moved, &tombstone) {
        let error = store.retire(generation, None).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{error}");
        assert!(fs::symlink_metadata(&tombstone).unwrap().is_symlink());
        fs::remove_file(&tombstone).unwrap();
    }
    fs::rename(&moved, &tombstone).unwrap();
    assert_eq!(store.retire(generation, None).unwrap(), Retirement::Retired);
    assert!(!tombstone.exists());
}

#[test]
fn directories_are_synced_where_the_platform_can() {
    let directory = tempfile::tempdir().unwrap();
    sync_dir(directory.path()).unwrap();
    // On Unix, a sync opens the directory, which must exist.
    #[cfg(unix)]
    {
        let error = sync_dir(&directory.path().join("missing")).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    }
}

#[test]
fn records_keep_their_json_form() {
    let (directory, store) = new_store();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let generation = published.generation();
    let sandbox = provision(directory.path(), "sandbox");
    let lease = leased(&store, &published, &sandbox);
    let expected = json!({
        "format": 1,
        "store": store.id().to_string(),
        "lease": lease.id().to_string(),
        "generation": generation.get().get(),
        "key": published.key().to_string(),
        "sandbox": sandbox.to_str().unwrap(),
        "launch": lease.launch().as_str(),
    });
    let record = store
        .dir()
        .join(LEASES_NAME)
        .join(format!("{}.json", lease.id()));
    assert_eq!(json_of(&record), expected);
    assert_eq!(json_of(&sandbox.join(CLONE_BINDING_NAME)), expected);
    store.tombstone(generation, None).unwrap();
    assert_eq!(
        json_of(
            &store
                .dir()
                .join(TOMBSTONES_NAME)
                .join(format!("{generation}.json"))
        ),
        json!({
            "format": 1,
            "store": store.id().to_string(),
            "generation": generation.get().get(),
        })
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        for name in [LEASES_NAME, TOMBSTONES_NAME] {
            let mode = fs::metadata(store.dir().join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700, "{name}");
        }
    }
    // A lease's identity is its text form.
    let text = lease.id().to_string();
    assert_eq!(text.len(), 32);
    assert_eq!(text.parse::<LeaseId>().unwrap(), lease.id());
    assert_eq!(text.to_uppercase().parse::<LeaseId>().unwrap(), lease.id());
    assert_eq!(LeaseId::from_bytes(*lease.id().as_bytes()), lease.id());
    for invalid in ["", "0123", &"g".repeat(32)] {
        let error = invalid.parse::<LeaseId>().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{invalid}");
    }
}

#[test]
fn malformed_records_and_foreign_entries_fail_clearly() {
    let (directory, store) = new_store();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let generation = published.generation();
    let sandbox = provision(directory.path(), "sandbox");
    let lease = leased(&store, &published, &sandbox);
    let record = store
        .dir()
        .join(LEASES_NAME)
        .join(format!("{}.json", lease.id()));
    let bound = sandbox.join(CLONE_BINDING_NAME);
    let valid = json_of(&record);
    let rewrite = |path: &Path, change: Change| {
        let mut value = valid.clone();
        change(&mut value);
        fs::write(path, value.to_string()).unwrap();
    };
    // A record of another format is neither read nor migrated.
    rewrite(&record, |value| value["format"] = json!(2));
    let error = store.leases(generation).unwrap_err();
    assert_eq!(error.code(), ErrorCode::FormatUnsupported, "{error}");
    // Records that are not leases.
    let malformed: [(Change, &str); 7] = [
        (
            |value| value["generation"] = json!(0),
            "it leases store generation 0",
        ),
        (
            |value| value["sandbox"] = json!("relative"),
            "is not an absolute path",
        ),
        (|value| value["key"] = json!("00"), "its key is invalid"),
        (
            |value| value["lease"] = json!("00"),
            "its identity is invalid",
        ),
        (
            |value| value["launch"] = json!("00"),
            "its launch is invalid",
        ),
        // A lease reserves its clone's launch.
        (
            |value| {
                value.as_object_mut().unwrap().remove("launch");
            },
            "is malformed",
        ),
        (|value| value["extra"] = json!(1), "is malformed"),
    ];
    for (change, problem) in malformed {
        rewrite(&bound, change);
        let error = Lease::of(&SandboxStore::open(&sandbox).unwrap()).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Io, "{error}");
        assert!(error.message().contains("is malformed"), "{error}");
        assert!(error.message().contains(problem), "{error}");
    }
    // A key that is not a key's identity fails as its parse does.
    rewrite(&bound, |value| value["key"] = json!("00"));
    let error = Lease::of(&SandboxStore::open(&sandbox).unwrap()).unwrap_err();
    let parse = "00".parse::<KeyIdentity>().unwrap_err();
    assert!(
        error
            .message()
            .ends_with(&format!("its key is invalid: {}", parse.message())),
        "{error}"
    );
    fs::write(&bound, valid.to_string()).unwrap();
    // A record of another lease than its name names, or of another store.
    let other_lease: Change = |value| value["lease"] = json!("0".repeat(32));
    let other_store: Change = |value| value["store"] = json!("0".repeat(32));
    for change in [other_lease, other_store] {
        rewrite(&record, change);
        let error = store.leases(generation).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Io, "{error}");
        assert!(error.message().contains("is malformed"), "{error}");
    }
    fs::write(&record, valid.to_string()).unwrap();
    assert_eq!(store.leases(generation).unwrap(), [lease]);
    // Entries that are not the store's, which a retirement refuses before it changes anything.
    let notes = store.dir().join(LEASES_NAME).join("notes.txt");
    fs::write(&notes, b"").unwrap();
    for error in [
        store.leases(generation).unwrap_err(),
        store.retire(generation, None).unwrap_err(),
    ] {
        assert_eq!(error.code(), ErrorCode::ForeignFiles, "{error}");
        assert!(
            error
                .message()
                .ends_with("holds entries that are not the store's: notes.txt"),
            "{error}"
        );
    }
    assert!(store.tombstones().unwrap().is_empty());
    fs::remove_file(&notes).unwrap();
    // A tombstone of another store, or of another format, which neither an acquisition nor a
    // listing or a selection takes for a quarantine, and entries that are not tombstones.
    store.tombstone(generation, None).unwrap();
    let tombstones = store.dir().join(TOMBSTONES_NAME);
    let tombstone = tombstones.join(format!("{generation}.json"));
    let valid_tombstone = json_of(&tombstone);
    let other = provision(directory.path(), "other");
    let foreign_tombstones: [(Change, ErrorCode); 2] = [
        (
            |value| value["store"] = json!("0".repeat(32)),
            ErrorCode::Io,
        ),
        (
            |value| value["format"] = json!(2),
            ErrorCode::FormatUnsupported,
        ),
    ];
    for (change, code) in foreign_tombstones {
        let mut value = valid_tombstone.clone();
        change(&mut value);
        fs::write(&tombstone, value.to_string()).unwrap();
        for error in [
            refusal(&store, generation, &other, &after(&published, 1)),
            store.tombstones().unwrap_err(),
            store
                .select(published.key(), &after(&published, 1))
                .unwrap_err(),
        ] {
            assert_eq!(error.code(), code, "{error}");
        }
    }
    fs::write(tombstones.join("+1.json"), b"").unwrap();
    for error in [
        store.tombstones().unwrap_err(),
        store
            .select(published.key(), &after(&published, 1))
            .unwrap_err(),
    ] {
        assert_eq!(error.code(), ErrorCode::ForeignFiles, "{error}");
        assert!(error.message().ends_with(": +1.json"), "{error}");
    }
}

#[test]
fn the_directories_of_leases_and_tombstones_refuse_links() {
    let (directory, store) = new_store();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let generation = published.generation();
    let sandbox = provision(directory.path(), "sandbox");
    let lease = leased(&store, &published, &sandbox);
    store.tombstone(generation, None).unwrap();
    for name in [LEASES_NAME, TOMBSTONES_NAME] {
        let path = store.dir().join(name);
        let moved = path.with_extension("moved");
        fs::rename(&path, &moved).unwrap();
        link_dir(&moved, &path);
        let mut errors = vec![store.retire(generation, None).unwrap_err()];
        if name == LEASES_NAME {
            errors.push(store.leases(generation).unwrap_err());
            errors.push(release(&store, &sandbox).unwrap_err());
        } else {
            errors.push(store.tombstones().unwrap_err());
            errors.push(
                store
                    .select(published.key(), &after(&published, 1))
                    .unwrap_err(),
            );
        }
        for error in errors {
            assert!(
                error.message().ends_with("is not a plain directory"),
                "{name}: {error}"
            );
        }
        unlink_dir(&path);
        fs::rename(&moved, &path).unwrap();
    }
    // Nothing changed meanwhile.
    assert_eq!(binding(&sandbox), Some(lease.clone()));
    assert_eq!(store.leases(generation).unwrap(), [lease]);
    assert_eq!(store.tombstones().unwrap(), [generation]);
}

/// A transaction in a process of its own, which [`transaction_role`] runs.
struct Transaction {
    /// The process.
    child: Child,
    /// Its standard input, a line on which starts the transaction.
    input: ChildStdin,
    /// Its standard output.
    output: BufReader<ChildStdout>,
}

impl Transaction {
    /// Starts the transaction `action` of the store generation `generation` of the store in
    /// `store`, for the sandbox in `sandbox`, if any, and returns once it is ready to run.
    fn start(
        store: &Path,
        action: &str,
        generation: StoreGeneration,
        sandbox: Option<&Path>,
    ) -> Self {
        let mut command = Command::new(env::current_exe().unwrap());
        command
            .args(role("transaction_role"))
            .env(ROLE_STORE, store)
            .env(ROLE_ACTION, action)
            .env(ROLE_GENERATION, generation.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        if let Some(sandbox) = sandbox {
            command.env(ROLE_SANDBOX, sandbox);
        }
        let mut child = supervisor::spawn_command(&mut command).unwrap();
        let input = child.stdin.take().unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        while line.trim_end() != READY {
            line.clear();
            let read = output.read_line(&mut line).unwrap();
            assert_ne!(read, 0, "the transaction ended before it was ready");
        }
        Self {
            child,
            input,
            output,
        }
    }

    /// Lets the transaction run.
    fn run(&mut self) {
        self.input.write_all(b"run\n").unwrap();
        self.input.flush().unwrap();
    }

    /// Waits for the transaction to end, and returns how it ended, as [`transaction_role`]
    /// prints it.
    fn outcome(mut self) -> String {
        let mut rest = String::new();
        self.output.read_to_string(&mut rest).unwrap();
        assert!(self.child.wait().unwrap().success(), "{rest}");
        rest.lines()
            .find_map(|line| line.strip_prefix(OUTCOME))
            .expect("the transaction printed its outcome")
            .to_owned()
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Runs `transactions` at once, with `other` amid them, and returns how each of `transactions`
/// ended, in their order, and how `other` ended.
fn race(mut transactions: Vec<Transaction>, mut other: Transaction) -> (Vec<String>, String) {
    let middle = transactions.len() / 2;
    for (index, transaction) in transactions.iter_mut().enumerate() {
        if index == middle {
            other.run();
        }
        transaction.run();
    }
    let outcomes = transactions.into_iter().map(Transaction::outcome).collect();
    (outcomes, other.outcome())
}

/// Returns the identities of the leases that `outcomes` report as acquired, sorted, and separated
/// by spaces.
fn acquired(outcomes: &[String]) -> String {
    let mut ids: Vec<&str> = outcomes
        .iter()
        .filter_map(|outcome| outcome.strip_prefix("acquired "))
        .collect();
    ids.sort_unstable();
    ids.join(" ")
}

/// A transaction in a process of its own, which [`Transaction::start`] runs: once a line arrives on
/// its standard input, it runs the transaction that [`ROLE_ACTION`] names, of the store generation
/// [`ROLE_GENERATION`] of the store in [`ROLE_STORE`], for the sandbox in [`ROLE_SANDBOX`], and
/// prints how it ended: `acquired ID`, `released ID` or `none`, `tombstoned`, `retired` or
/// `draining ID...`, or the code of its error.
#[test]
#[ignore = "a transaction that the lease tests run in a process of its own"]
fn transaction_role() {
    let (Some(dir), Ok(action), Ok(generation)) = (
        env::var_os(ROLE_STORE),
        env::var(ROLE_ACTION),
        env::var(ROLE_GENERATION),
    ) else {
        return;
    };
    let store = Store::open(dir).unwrap();
    let generation = StoreGeneration::new(generation.parse().unwrap());
    let sandbox = env::var_os(ROLE_SANDBOX).map(PathBuf::from);
    // A second after the capture of the generation, on the host that captured it.
    let now = store
        .index()
        .unwrap()
        .get(generation)
        .map(|indexed| after(indexed, 1));
    // The test harness has printed the test's name without ending the line.
    println!("\n{READY}");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();
    let deadline = Some(Instant::now() + Duration::from_secs(30));
    let code = |error: Error| error.code().to_string();
    let outcome = match action.as_str() {
        "acquire" => store
            .acquire(
                generation,
                sandbox.unwrap(),
                &files(),
                &now.unwrap(),
                deadline,
            )
            .map_or_else(code, |acquired| format!("acquired {}", acquired.lease.id())),
        "release" => {
            let mut locked =
                SandboxStore::lock_existing(sandbox.unwrap(), &files(), deadline).unwrap();
            match store.release(&mut locked) {
                Ok(Some(lease)) => format!("released {}", lease.id()),
                Ok(None) => "none".to_owned(),
                Err(error) => code(error),
            }
        }
        "tombstone" => store
            .tombstone(generation, deadline)
            .map_or_else(code, |()| "tombstoned".to_owned()),
        "retire" => match store.retire(generation, deadline) {
            Ok(Retirement::Retired) => "retired".to_owned(),
            Ok(Retirement::Draining(leases)) => format!("draining {}", ids(&leases)),
            Err(error) => code(error),
        },
        _ => panic!("unknown transaction {action}"),
    };
    println!("{OUTCOME}{outcome}");
}

#[test]
fn processes_that_race_to_lease_one_sandbox_lease_it_once() {
    /// The processes that race.
    const PROCESSES: usize = 6;
    let (directory, store) = new_store();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let generation = published.generation();
    let sandbox = provision(directory.path(), "sandbox");
    let mut acquisitions: Vec<Transaction> = (0..PROCESSES)
        .map(|_| Transaction::start(store.dir(), "acquire", generation, Some(&sandbox)))
        .collect();
    for acquisition in &mut acquisitions {
        acquisition.run();
    }
    let outcomes: Vec<String> = acquisitions.into_iter().map(Transaction::outcome).collect();
    let leases = store.leases(generation).unwrap();
    assert_eq!(leases.len(), 1, "{outcomes:?}");
    assert_eq!(acquired(&outcomes), leases[0].id().to_string());
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| *outcome == "sandbox-leased")
            .count(),
        PROCESSES - 1,
        "{outcomes:?}"
    );
    assert_eq!(binding(&sandbox), Some(leases[0].clone()));
}

#[test]
fn a_tombstone_that_races_acquisitions_in_other_processes_keeps_the_leases_before_it() {
    /// The clones whose acquisitions race the tombstone.
    const CLONES: usize = 6;
    let (directory, store) = new_store();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let generation = published.generation();
    let sandboxes: Vec<PathBuf> = (0..CLONES)
        .map(|clone| provision(directory.path(), &format!("clone-{clone}")))
        .collect();
    let acquisitions = sandboxes
        .iter()
        .map(|sandbox| Transaction::start(store.dir(), "acquire", generation, Some(sandbox)))
        .collect();
    let tombstone = Transaction::start(store.dir(), "tombstone", generation, None);
    let (outcomes, tombstoned) = race(acquisitions, tombstone);
    assert_eq!(tombstoned, "tombstoned");
    // Each acquisition leased the generation before the tombstone, or found it quarantined after.
    for (sandbox, outcome) in sandboxes.iter().zip(&outcomes) {
        match outcome.strip_prefix("acquired ") {
            Some(id) => assert_eq!(binding(sandbox).unwrap().id().to_string(), id),
            None => {
                assert_eq!(outcome, "template-tombstoned");
                assert_eq!(binding(sandbox), None);
                let error = refusal(&store, generation, sandbox, &after(&published, 1));
                assert_eq!(error.code(), ErrorCode::TemplateTombstoned, "{error}");
            }
        }
    }
    // The leases granted before it keep working.
    assert_eq!(ids(&store.leases(generation).unwrap()), acquired(&outcomes));
    assert_eq!(store.tombstones().unwrap(), [generation]);
}

#[test]
fn a_retirement_that_races_acquisitions_and_releases_in_other_processes_waits_for_its_leases() {
    /// The clones whose acquisitions and releases race the retirements.
    const CLONES: usize = 6;
    let (directory, store) = new_store();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let generation = published.generation();
    let sandboxes: Vec<PathBuf> = (0..CLONES)
        .map(|clone| provision(directory.path(), &format!("clone-{clone}")))
        .collect();
    let start = |action: &str, sandbox: Option<&Path>| {
        Transaction::start(store.dir(), action, generation, sandbox)
    };
    let acquisitions = sandboxes
        .iter()
        .map(|sandbox| start("acquire", Some(sandbox)))
        .collect();
    let (outcomes, retired) = race(acquisitions, start("retire", None));
    // The retirement drains the leases of the acquisitions before it, and those after it find
    // the generation quarantined, or gone if none leased it before.
    let granted = acquired(&outcomes);
    if granted.is_empty() {
        assert_eq!(retired, "retired");
        assert!(
            outcomes.iter().all(|outcome| outcome == "not-found"),
            "{outcomes:?}"
        );
    } else {
        assert_eq!(retired, format!("draining {granted}"));
        assert!(
            outcomes.iter().all(|outcome| outcome.starts_with("acquired ")
                || outcome == "template-tombstoned"),
            "{outcomes:?}"
        );
        assert!(store.snapshot_dir(&published).is_ok());
    }
    assert_eq!(ids(&store.leases(generation).unwrap()), granted);
    // The clones release their leases while another retirement runs, which deletes the
    // generation only once none remains.
    let releases = sandboxes
        .iter()
        .map(|sandbox| start("release", Some(sandbox)))
        .collect();
    let (released, retired) = race(releases, start("retire", None));
    for (outcome, acquisition) in released.iter().zip(&outcomes) {
        match acquisition.strip_prefix("acquired ") {
            Some(id) => assert_eq!(outcome, &format!("released {id}")),
            None => assert_eq!(outcome, "none"),
        }
    }
    assert!(
        retired == "retired" || retired.starts_with("draining "),
        "{retired}"
    );
    assert_eq!(store.retire(generation, None).unwrap(), Retirement::Retired);
    assert!(!store.payload_dir(generation).exists());
    assert_eq!(store.index().unwrap().get(generation), None);
    assert!(store.leases(generation).unwrap().is_empty());
    assert!(store.tombstones().unwrap().is_empty());
}

/// A clone's orchestrator in a process of its own, which [`run_clone_role`] runs: it leases the
/// store generation [`ROLE_GENERATION`] of the store in [`ROLE_STORE`] to the sandbox in
/// [`ROLE_SANDBOX`], and launches the clone with a [`vmm_role`] in [`ROLE_VMM`] as its VMM, or it
/// releases the sandbox's lease; and it exits without unwinding after the step that [`ROLE_CRASH`]
/// names, as a crash would, once it has printed the reference of the VMM that it spawned, if any.
#[test]
#[ignore = "a clone's orchestrator that the lease tests run in a process of its own"]
fn clone_role() {
    let (Some(dir), Ok(generation), Some(sandbox), Ok(step), Some(vmm)) = (
        env::var_os(ROLE_STORE),
        env::var(ROLE_GENERATION),
        env::var_os(ROLE_SANDBOX).map(PathBuf::from),
        env::var(ROLE_CRASH),
        env::var_os(ROLE_VMM).map(PathBuf::from),
    ) else {
        return;
    };
    let store = Store::open(dir).unwrap();
    let generation = StoreGeneration::new(generation.parse().unwrap());
    // The step's transaction crashes as it makes its write in `target` durable, or, for a
    // release, the removal of the binding, in the sandbox's directory.
    let target = match step.as_str() {
        "after-record" => Some(store.dir().join(LEASES_NAME)),
        "after-binding" => Some(sandbox.clone()),
        _ => None,
    };
    let sync = |dir: &Path| -> Result<()> {
        if target.as_deref() == Some(dir) {
            crash(&step);
        }
        sync_dir(dir)
    };
    if step == "releasing" {
        let mut locked = SandboxStore::lock_existing(&sandbox, &files(), None).unwrap();
        store
            .release_with(&mut locked, &|| {}, &platform::Directory::sync, &|_| {
                crash(&step)
            })
            .unwrap();
        panic!("the release did not make the removal of its binding durable");
    }
    let now = after(store.index().unwrap().get(generation).unwrap(), 1);
    let mut acquired = store
        .acquire_with(generation, sandbox.clone(), &files(), &now, None, &sync)
        .unwrap();
    let reserved = acquired.lease.launch().clone();
    let command = |_: &str| -> Result<VmmCommand<'static>> {
        Ok(VmmCommand::new(
            env::current_exe().unwrap(),
            role("vmm_role"),
            &vmm,
        ))
    };
    match step.as_str() {
        "acquired" => crash(&step),
        "before-spawn" => {
            let _ = supervisor::launch(
                &mut acquired.locked,
                &reserved,
                &runtime(),
                &CAPABILITY,
                |_| crash(&step),
            );
        }
        "after-spawn" => {
            // The steps of `supervisor::launch`, up to the spawn.
            let mut launch = acquired
                .locked
                .begin_launch(&reserved, &runtime(), &CAPABILITY)
                .unwrap();
            let command = command(launch.endpoint()).unwrap();
            let log = launch.take_log().unwrap();
            let launched = supervisor::spawn(&command, &CAPABILITY, log).unwrap();
            print_reference(launched.process().unwrap().reference());
            crash(&step);
        }
        "after-identity" => {
            let starting = supervisor::launch(
                &mut acquired.locked,
                &reserved,
                &runtime(),
                &CAPABILITY,
                command,
            )
            .unwrap();
            print_reference(starting.process().reference());
            crash(&step);
        }
        "committed" => {
            let starting = supervisor::launch(
                &mut acquired.locked,
                &reserved,
                &runtime(),
                &CAPABILITY,
                command,
            )
            .unwrap();
            let running = starting.commit().unwrap();
            print_reference(running.record().process());
            crash(&step);
        }
        _ => {}
    }
    panic!("the clone's orchestrator did not crash after {step}");
}

/// The VMM of a clone of [`clone_role`]: reads its capability as OpenVMM does, then runs until a
/// file named [`EXIT_MARKER`] appears in its working directory, which [`VMM_MARKER`] marks, or for
/// [`VMM_LIFETIME`] at most.
#[test]
#[ignore = "a VMM that the lease tests run in a process of its own"]
fn vmm_role() {
    let directory = env::current_dir().unwrap();
    if !directory.join(VMM_MARKER).exists() {
        return;
    }
    let mut capability = [0; CAPABILITY_LEN];
    std::io::stdin().read_exact(&mut capability).unwrap();
    assert_eq!(capability, CAPABILITY);
    let started = Instant::now();
    while !directory.join(EXIT_MARKER).exists() && started.elapsed() < VMM_LIFETIME {
        thread::sleep(Duration::from_millis(10));
    }
}

/// Prints `reference` as JSON, on a line of its own, after [`REFERENCE`].
fn print_reference(reference: &ProcessRef) {
    println!("\n{REFERENCE}{}", serde_json::to_string(reference).unwrap());
}

/// Ends this process at once, once it has printed that it crashes after `step`, without unwinding
/// or running destructors, as a crash would.
fn crash(step: &str) -> ! {
    println!("\n{CRASHED}{step}");
    std::io::stdout().flush().unwrap();
    std::process::exit(0)
}

/// Runs [`clone_role`] for the generation `generation` of `store`, the sandbox in `sandbox`, and a
/// VMM in `vmm`, until it crashes after `step`, and returns the reference of the VMM that it
/// spawned, if it spawned one.
fn run_clone_role(
    step: &str,
    store: &Store,
    generation: StoreGeneration,
    sandbox: &Path,
    vmm: &Path,
) -> Option<ProcessRef> {
    let mut command = Command::new(env::current_exe().unwrap());
    command
        .args(role("clone_role"))
        .env(ROLE_STORE, store.dir())
        .env(ROLE_GENERATION, generation.to_string())
        .env(ROLE_SANDBOX, sandbox)
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

/// The working directory of a [`vmm_role`], which it marks. Dropping it ends the VMM, and waits
/// for it to exit, so that a failed test leaves no process behind.
struct VmmDir {
    /// The reference of the VMM, once the test knows it.
    reference: Option<ProcessRef>,
    /// The directory, which is removed after the VMM exits.
    dir: tempfile::TempDir,
}

impl VmmDir {
    /// Creates the directory, and marks it.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(VMM_MARKER), b"").unwrap();
        Self {
            reference: None,
            dir,
        }
    }

    /// Returns the path of the directory.
    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Asks the VMM to exit.
    fn end(&self) {
        fs::write(self.path().join(EXIT_MARKER), b"").unwrap();
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

/// A clone whose orchestrator, [`clone_role`], crashed after a step of the clone's launch, in a
/// fresh temporary directory: its VMM's working directory, its template store and sandbox, and
/// the lease that the sandbox holds.
struct Crashed {
    /// The working directory of the clone's VMM, which ends the VMM as it drops, before the rest.
    vmm: VmmDir,
    /// The template store.
    store: Store,
    /// The sandbox's directory.
    sandbox: PathBuf,
    /// The lease that the sandbox holds.
    lease: Lease,
    /// The temporary directory of the store and the sandbox.
    _directory: tempfile::TempDir,
}

impl Crashed {
    /// Publishes a generation in a new store, and leases it to a new sandbox, whose clone
    /// [`clone_role`] launches until it crashes after `step`.
    fn after(step: &str) -> Self {
        let (directory, store) = new_store();
        let published = publish(&store, &ManifestSpec::template(), storage(1));
        let sandbox = provision(directory.path(), "sandbox");
        let mut vmm = VmmDir::new();
        vmm.reference = run_clone_role(step, &store, published.generation(), &sandbox, vmm.path());
        let lease = binding(&sandbox).expect("the crash kept the clone's lease");
        Self {
            vmm,
            store,
            sandbox,
            lease,
            _directory: directory,
        }
    }

    /// Takes the lock of the clone's sandbox, as its restarted orchestrator does.
    fn lock(&self) -> LockedStore {
        SandboxStore::lock_existing(&self.sandbox, &files(), Some(Instant::now() + TIMEOUT))
            .unwrap()
    }

    /// Checks that the store refuses to release the lease of the sandbox of `locked`, whose VM
    /// may still run, and that the sandbox keeps it.
    fn refuses_release(&self, locked: &mut LockedStore) {
        let error = self.store.release(locked).unwrap_err();
        assert_eq!(error.code(), ErrorCode::SandboxRunning, "{error}");
        assert_eq!(binding(&self.sandbox), Some(self.lease.clone()));
    }

    /// Checks that the store releases the lease of the sandbox of `locked`, and that the lease is
    /// gone.
    fn releases(&self, locked: &mut LockedStore) {
        assert_eq!(
            self.store.release(locked).unwrap(),
            Some(self.lease.clone())
        );
        assert_eq!(binding(&self.sandbox), None);
        assert!(
            self.store
                .leases(self.lease.generation())
                .unwrap()
                .is_empty()
        );
    }
}

/// Reattaches to the VM of the sandbox of `locked` until the result is determinate, as a caller
/// retries an indeterminate reattach, and returns it.
///
/// On Windows, a scanner that briefly opens the log of a launch can keep its claim held for a
/// moment.
fn reattach_until_determinate(locked: &mut LockedStore) -> Reattached {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match supervisor::reattach(locked, &runtime()).unwrap() {
            Reattached::Indeterminate(reason) if Instant::now() < deadline => {
                eprintln!("reattach is indeterminate: {reason}");
                thread::sleep(Duration::from_millis(100));
            }
            reattached => return reattached,
        }
    }
}

#[test]
fn a_crash_during_an_acquisition_leaves_no_lease_or_one_that_launches_nothing() {
    let (directory, store) = new_store();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let generation = published.generation();
    // No VMM runs: the orchestrator crashes before any launch.
    let vmm = directory.path();
    // After the record, before the binding: the sandbox holds no lease, and leases the generation
    // again, while the record, which no binding names, keeps the generation.
    let unbound = provision(directory.path(), "unbound");
    assert_eq!(
        run_clone_role("after-record", &store, generation, &unbound, vmm),
        None
    );
    assert_eq!(binding(&unbound), None);
    let [orphan]: [Lease; 1] = store.leases(generation).unwrap().try_into().unwrap();
    assert_eq!(orphan.sandbox(), unbound);
    let mut leases = vec![leased(&store, &published, &unbound)];
    // After the binding, before or after the acquisition returned: the sandbox holds the lease,
    // whose launch no later lock begins, and which a release ends. Another acquisition then
    // reserves another launch.
    for step in ["after-binding", "acquired"] {
        let sandbox = provision(directory.path(), step);
        assert_eq!(
            run_clone_role(step, &store, generation, &sandbox, vmm),
            None
        );
        let lease = binding(&sandbox).unwrap();
        assert!(store.leases(generation).unwrap().contains(&lease), "{step}");
        let mut locked = SandboxStore::lock_existing(&sandbox, &files(), None).unwrap();
        let error = launch_refusal(&mut locked, lease.launch());
        assert_eq!(error.code(), ErrorCode::SandboxLeased, "{step}: {error}");
        assert_eq!(store.release(&mut locked).unwrap(), Some(lease.clone()));
        drop(locked);
        let again = leased(&store, &published, &sandbox);
        assert_ne!(again.launch(), lease.launch());
        leases.push(again);
    }
    // A retirement removes the record that no binding names, and waits for the leases.
    leases.sort_by_key(Lease::id);
    assert_eq!(
        store.retire(generation, None).unwrap(),
        Retirement::Draining(leases.clone())
    );
    assert_eq!(store.leases(generation).unwrap(), leases);
}

#[test]
fn a_clone_that_crashes_before_its_spawn_keeps_its_lease_until_its_launch_ends() {
    let crashed = Crashed::after("before-spawn");
    assert_eq!(crashed.vmm.reference, None);
    let mut locked = crashed.lock();
    // The journal names the launch that the lease reserved, which spawned nothing.
    let journal = locked.journal().unwrap().unwrap();
    assert_eq!(journal.launch(), crashed.lease.launch());
    assert_eq!(journal.state(), LaunchState::Prepared);
    crashed.refuses_release(&mut locked);
    match reattach_until_determinate(&mut locked) {
        Reattached::Exited {
            record: None,
            recovery: Recovery::Exited,
        } => {}
        other => panic!("unexpected reattach: {other:?}"),
    }
    crashed.releases(&mut locked);
}

#[test]
fn a_clone_that_crashes_after_its_spawn_keeps_its_lease_until_its_unrecorded_vmm_exits() {
    let crashed = Crashed::after("after-spawn");
    let reference = crashed.vmm.reference.clone().unwrap();
    let process = Process::open(&reference).unwrap().unwrap();
    let mut locked = crashed.lock();
    assert_eq!(
        locked.journal().unwrap().unwrap().state(),
        LaunchState::Prepared
    );
    // Reattach can neither prove that the VMM, which no record names, exited, nor verify it: it
    // signals nothing, and the lease stays.
    let Reattached::Indeterminate(_) = supervisor::reattach(&mut locked, &runtime()).unwrap()
    else {
        panic!("reattach ended a launch whose VMM runs unverified");
    };
    assert!(!process.has_exited().unwrap());
    crashed.refuses_release(&mut locked);
    // Once the VMM exits, the claim that it released proves it.
    crashed.vmm.end();
    assert!(process.wait(Instant::now() + TIMEOUT).unwrap());
    match reattach_until_determinate(&mut locked) {
        Reattached::Exited {
            record: None,
            recovery: Recovery::Exited,
        } => {}
        other => panic!("unexpected reattach: {other:?}"),
    }
    crashed.releases(&mut locked);
}

#[test]
fn a_clone_that_crashes_after_recording_its_vmm_keeps_its_lease_until_reattach_ends_it() {
    let crashed = Crashed::after("after-identity");
    let reference = crashed.vmm.reference.clone().unwrap();
    let process = Process::open(&reference).unwrap().unwrap();
    let mut locked = crashed.lock();
    let journal = locked.journal().unwrap().unwrap();
    assert_eq!(journal.launch(), crashed.lease.launch());
    assert_eq!(journal.state(), LaunchState::Spawned);
    assert_eq!(journal.process(), Some(&reference));
    crashed.refuses_release(&mut locked);
    // The launch never committed, so reattach ends its VMM through its verified reference.
    match supervisor::reattach(&mut locked, &runtime()).unwrap() {
        Reattached::Exited {
            record: None,
            recovery: Recovery::Terminated(terminated),
        } => assert_eq!(terminated, reference),
        other => panic!("unexpected reattach: {other:?}"),
    }
    assert!(process.wait(Instant::now() + TIMEOUT).unwrap());
    crashed.releases(&mut locked);
}

#[test]
fn a_committed_clone_keeps_its_lease_through_a_crash_until_its_vm_stops() {
    let crashed = Crashed::after("committed");
    let reference = crashed.vmm.reference.clone().unwrap();
    let mut locked = crashed.lock();
    crashed.refuses_release(&mut locked);
    // The restarted orchestrator holds the clone's VM again, and its lease lasts while it runs.
    let Reattached::Live(running) = supervisor::reattach(&mut locked, &runtime()).unwrap() else {
        panic!("the committed clone was not reattached");
    };
    assert_eq!(running.record().launch(), crashed.lease.launch());
    assert_eq!(running.record().process(), &reference);
    crashed.refuses_release(&mut locked);
    let stopped = running
        .stop(
            &mut locked,
            Instant::now() + TIMEOUT,
            None,
            |_running, _deadline| {
                crashed.vmm.end();
                Ok::<(), Infallible>(())
            },
        )
        .unwrap();
    assert_eq!(stopped.outcome, StopOutcome::Graceful);
    crashed.releases(&mut locked);
}

#[test]
fn a_crash_during_a_release_leaves_a_record_that_a_retirement_removes() {
    let (directory, store) = new_store();
    let published = publish(&store, &ManifestSpec::template(), storage(1));
    let generation = published.generation();
    let sandbox = provision(directory.path(), "sandbox");
    let lease = leased(&store, &published, &sandbox);
    assert_eq!(
        run_clone_role("releasing", &store, generation, &sandbox, directory.path()),
        None
    );
    // The binding went first: the sandbox holds no lease, while the record, which no binding
    // names, keeps the generation until a retirement removes it.
    assert_eq!(binding(&sandbox), None);
    assert_eq!(store.leases(generation).unwrap(), [lease]);
    assert_eq!(release(&store, &sandbox).unwrap(), None);
    assert_eq!(store.retire(generation, None).unwrap(), Retirement::Retired);
    assert!(store.leases(generation).unwrap().is_empty());
}

/// Tests of the records that a Windows file system without POSIX deletion keeps under their names
/// while other handles hold them, after a transaction in another process removed them, and of the
/// records whose access control lists refuse access, which are not removed ones.
#[cfg(windows)]
#[allow(unsafe_code)]
mod windows {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_DISPOSITION_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FileDispositionInfo, SetFileInformationByHandle,
    };

    use super::*;
    use crate::host::store::lease::BINDING_DESCRIPTION;

    /// Deletes the file `path` as a file system without POSIX deletion deletes a file that other
    /// handles hold: its name stays, and opening it fails, until the last handle closes, of which
    /// the returned one is the last unless others hold the file.
    fn delete_pending(path: &Path) -> fs::File {
        let file = fs::OpenOptions::new()
            .access_mode(DELETE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(path)
            .unwrap();
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: the handle is open, and the information is a FILE_DISPOSITION_INFO of its size.
        let set = unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle() as HANDLE,
                FileDispositionInfo,
                (&raw const disposition).cast(),
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        };
        assert_ne!(set, 0, "{}", io::Error::last_os_error());
        file
    }

    /// The access control list of a file that allows no one anything, which its owner may still
    /// change: every open of the file fails, also one without access rights, unless the process's
    /// privileges bypass the list.
    const NO_ACCESS: &str = "D:P";

    /// The access control list of a file that allows its owner alone, as the store's files do.
    const OWNER_ONLY: &str = "D:P(A;;FA;;;OW)";

    #[test]
    fn only_a_file_that_is_gone_or_whose_deletion_is_pending_was_removed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("record.json");
        fs::write(&path, b"{}").unwrap();
        let denied = io::Error::from(io::ErrorKind::PermissionDenied);
        assert!(!was_removed(&path, &denied));
        // A file whose list refuses every open, also one without access rights, is not removed.
        // Nor is it where the process's privileges bypass the list, as the backup privilege lets
        // the open of `file_identity`, which has backup semantics, through.
        platform::set_access(&path, NO_ACCESS).unwrap();
        if let Err(error) = platform::file_identity(&path) {
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
        }
        assert!(!was_removed(&path, &denied));
        platform::set_access(&path, OWNER_ONLY).unwrap();
        // One whose deletion is pending is, as is one that is gone, but only a denied access tells
        // so.
        let held = delete_pending(&path);
        assert!(was_removed(&path, &denied));
        assert!(!was_removed(&path, &io::Error::other("the open failed")));
        drop(held);
        assert!(was_removed(&path, &denied));
        // So is a file whose directory is gone.
        let gone = directory.path().join("gone").join("record.json");
        assert!(was_removed(&gone, &denied));
    }

    #[test]
    fn records_that_refuse_access_are_not_taken_for_removed_ones() {
        let (directory, store) = new_store();
        let published = publish(&store, &ManifestSpec::template(), storage(1));
        let generation = published.generation();
        let sandbox = provision(directory.path(), "sandbox");
        let lease = leased(&store, &published, &sandbox);
        let record = store
            .dir()
            .join(LEASES_NAME)
            .join(format!("{}.json", lease.id()));
        // A retirement that cannot read the record of a lease, or its clone binding, cannot tell
        // whether the lease is live, and fails, keeping the generation.
        for (path, description) in [
            (record, LEASE_DESCRIPTION),
            (sandbox.join(CLONE_BINDING_NAME), BINDING_DESCRIPTION),
        ] {
            platform::set_access(&path, NO_ACCESS).unwrap();
            let error = store.retire(generation, None).unwrap_err();
            assert!(
                error
                    .message()
                    .starts_with(&format!("cannot read the {description} ")),
                "{error}"
            );
            platform::set_access(&path, OWNER_ONLY).unwrap();
        }
        assert_eq!(
            store.retire(generation, None).unwrap(),
            Retirement::Draining(vec![lease.clone()])
        );
        assert_eq!(binding(&sandbox), Some(lease));
        // A tombstone that refuses access quarantines its generation still.
        let other = publish(&store, &ManifestSpec::template(), storage(2));
        store.tombstone(other.generation(), None).unwrap();
        let tombstone = store
            .dir()
            .join(TOMBSTONES_NAME)
            .join(format!("{}.json", other.generation()));
        platform::set_access(&tombstone, NO_ACCESS).unwrap();
        let now = after(&other, 1);
        let leasing = provision(directory.path(), "leasing");
        for error in [
            store.tombstones().unwrap_err(),
            store.select(other.key(), &now).unwrap_err(),
            refusal(&store, other.generation(), &leasing, &now),
        ] {
            assert!(
                error
                    .message()
                    .starts_with(&format!("cannot read the {TOMBSTONE_DESCRIPTION} ")),
                "{error}"
            );
        }
        assert_eq!(binding(&leasing), None);
        platform::set_access(&tombstone, OWNER_ONLY).unwrap();
        assert_eq!(
            store.tombstones().unwrap(),
            [generation, other.generation()]
        );
    }

    #[test]
    fn records_that_another_transaction_removes_are_gone_for_their_readers() {
        let (directory, store) = new_store();
        let published = publish(&store, &ManifestSpec::template(), storage(1));
        let generation = published.generation();
        let sandbox = provision(directory.path(), "sandbox");
        let lease = leased(&store, &published, &sandbox);
        let record = store
            .dir()
            .join(LEASES_NAME)
            .join(format!("{}.json", lease.id()));
        // A release in another process removed the binding, and is removing the record, which a
        // scanner, say, holds open.
        let held_binding = delete_pending(&sandbox.join(CLONE_BINDING_NAME));
        let held_record = delete_pending(&record);
        assert_eq!(binding(&sandbox), None);
        assert!(store.leases(generation).unwrap().is_empty());
        // A tombstone that a retirement removes, likewise.
        let other = publish(&store, &ManifestSpec::template(), storage(2));
        store.tombstone(other.generation(), None).unwrap();
        let tombstone = store
            .dir()
            .join(TOMBSTONES_NAME)
            .join(format!("{}.json", other.generation()));
        let held_tombstone = delete_pending(&tombstone);
        assert!(store.tombstones().unwrap().is_empty());
        assert_eq!(
            store.select(other.key(), &after(&other, 1)).unwrap(),
            Some(other.clone())
        );
        // A retirement finds no lease of the generation.
        assert_eq!(store.retire(generation, None).unwrap(), Retirement::Retired);
        drop((held_binding, held_record, held_tombstone));
        assert!(!record.exists());
        assert!(!tombstone.exists());
    }

    #[test]
    fn a_release_whose_record_another_transaction_removes_ends_its_lease() {
        let (directory, store) = new_store();
        let published = publish(&store, &ManifestSpec::template(), storage(1));
        let generation = published.generation();
        let sandbox = provision(directory.path(), "sandbox");
        let lease = leased(&store, &published, &sandbox);
        let record = store
            .dir()
            .join(LEASES_NAME)
            .join(format!("{}.json", lease.id()));
        // A retirement in another process is removing the record, as the release does.
        let held_record = delete_pending(&record);
        assert_eq!(release(&store, &sandbox).unwrap(), Some(lease));
        assert_eq!(binding(&sandbox), None);
        drop(held_record);
        assert!(store.leases(generation).unwrap().is_empty());
    }

    #[test]
    fn a_release_takes_the_files_of_its_sandbox_whose_deletion_is_pending_for_removed() {
        let (directory, store) = new_store();
        let published = publish(&store, &ManifestSpec::template(), storage(1));
        let generation = published.generation();
        let sandbox = provision(directory.path(), "sandbox");
        let lease = leased(&store, &published, &sandbox);
        // A stop removed the runtime record of the clone's VM, and the journal of its launch,
        // which a scanner, say, holds: the release, which reads them through its handle of the
        // sandbox's directory, finds that the sandbox records no VM and no launch.
        let held: Vec<fs::File> = [JOURNAL_NAME, RUNTIME_NAME]
            .into_iter()
            .map(|name| {
                fs::write(sandbox.join(name), b"{}").unwrap();
                delete_pending(&sandbox.join(name))
            })
            .collect();
        assert_eq!(release(&store, &sandbox).unwrap(), Some(lease));
        assert_eq!(binding(&sandbox), None);
        assert!(store.leases(generation).unwrap().is_empty());
        drop(held);
        // A release that removed the binding, which a reader holds, and failed before its record
        // went, leaves a binding whose deletion is pending, which a release retried finds removed.
        let again = leased(&store, &published, &sandbox);
        let held_binding = delete_pending(&sandbox.join(CLONE_BINDING_NAME));
        assert_eq!(release(&store, &sandbox).unwrap(), None);
        drop(held_binding);
        assert_eq!(binding(&sandbox), None);
        assert_eq!(store.leases(generation).unwrap(), [again]);
        assert_eq!(store.retire(generation, None).unwrap(), Retirement::Retired);
    }
}
