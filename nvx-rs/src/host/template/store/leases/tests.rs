//! Tests of leases, clone bindings, tombstones, and retirement, by one process, and by
//! transactions in processes of their own, which [`transaction_role`] runs.

use std::cell::{Cell, RefCell};
use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

use super::*;
use crate::host::store::files::write_private;
use crate::host::store::{FORMAT, RUNTIME_NAME, Recovery, RuntimeIdentity};
use crate::host::template::eligibility::MAX_DOWNTIME_NS;
use crate::host::template::store::tests::{
    link_dir, link_file, publish, publish_with, remove, storage, store as new_store, unlink_dir,
};
use crate::host::template::test_support::{ManifestSpec, artifact, network_class};
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

/// Returns the files that these tests' callers keep in their sandboxes.
fn files() -> CallerFiles {
    CallerFiles::new(CONFIG).unwrap()
}

/// Provisions the sandbox `name` in `parent`, and returns its directory.
fn provision(parent: &Path, name: &str) -> PathBuf {
    let dir = parent.join(name);
    let locked = SandboxStore::lock_for_provision(&dir, &files(), None).unwrap();
    fs::write(dir.join(CONFIG), b"{}").unwrap();
    drop(locked);
    dir
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
            .begin_launch(&runtime(), &[7; CAPABILITY_LEN])
            .unwrap(),
    );
    drop(locked);
    let error = refusal(&store, generation, &sandbox, &now);
    assert_eq!(error.code(), ErrorCode::SandboxRunning, "{error}");
    let mut locked = SandboxStore::lock_existing(&sandbox, &files(), None).unwrap();
    assert_eq!(locked.recover().unwrap(), Recovery::Exited);
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
            .begin_launch(&runtime(), &[7; CAPABILITY_LEN])
            .unwrap(),
    );
    let error = store.release(&mut acquired.locked).unwrap_err();
    assert_eq!(error.code(), ErrorCode::SandboxRunning, "{error}");
    assert_eq!(acquired.locked.recover().unwrap(), Recovery::Exited);
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
    // Each sync notes its directory, and whether the binding and the record existed then.
    let synced = RefCell::new(Vec::new());
    let sync = |dir: &Path| -> Result<()> {
        let bound = sandbox.join(CLONE_BINDING_NAME).exists();
        synced
            .borrow_mut()
            .push((dir.to_owned(), bound, record.exists()));
        Ok(())
    };
    let mut locked = SandboxStore::lock_existing(&sandbox, &files(), None).unwrap();
    assert_eq!(store.release_with(&mut locked, &sync).unwrap(), Some(lease));
    drop(locked);
    assert_eq!(*synced.borrow(), [(sandbox.clone(), false, true)]);
    assert!(!record.exists());
    // A release that cannot make the binding's removal durable keeps the record, which a
    // retirement removes once no binding names it.
    let again = leased(&store, &published, &sandbox);
    let mut locked = SandboxStore::lock_existing(&sandbox, &files(), None).unwrap();
    let error = store
        .release_with(&mut locked, &|_: &Path| Err(Error::io("the disk failed")))
        .unwrap_err();
    drop(locked);
    assert_eq!(error.message(), "the disk failed");
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
    let first = publish(&store, &ManifestSpec::template(), storage(1));
    let second = publish(&store, &ManifestSpec::template(), storage(2));
    let third = publish(&store, &ManifestSpec::template(), storage(3));
    let fourth = publish(&store, &ManifestSpec::template(), storage(4));
    let released = provision(directory.path(), "released");
    let removed = provision(directory.path(), "removed");
    let reprovisioned = provision(directory.path(), "reprovisioned");
    // A sandbox whose parent the path names through a link.
    let sandboxes = directory.path().join("sandboxes");
    fs::create_dir(&sandboxes).unwrap();
    let linked = directory.path().join("linked");
    link_dir(&sandboxes, &linked);
    let removed_through_link = provision(&linked, "removed");
    let released_lease = leased(&store, &first, &released);
    let removed_lease = leased(&store, &second, &removed);
    let linked_lease = leased(&store, &third, &removed_through_link);
    let reprovisioned_lease = leased(&store, &fourth, &reprovisioned);
    // A release that removed its binding, but did not make that durable, and removals of
    // sandboxes, one of which is provisioned again at its path.
    fs::remove_file(released.join(CLONE_BINDING_NAME)).unwrap();
    for sandbox in [&removed, &removed_through_link, &reprovisioned] {
        let locked = SandboxStore::lock_existing(sandbox, &files(), None).unwrap();
        assert!(locked.remove().unwrap().is_none());
    }
    assert_eq!(provision(directory.path(), "reprovisioned"), reprovisioned);
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
            vec![released.as_path(), directory.path()],
        ),
        (&second, &removed_lease, vec![directory.path()]),
        (&third, &linked_lease, vec![linked.as_path()]),
        (
            &fourth,
            &reprovisioned_lease,
            vec![reprovisioned.as_path(), directory.path()],
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
    let dangling = provision(&linked, "dangling");
    let dangling_lease = leased(&store, &fifth, &dangling);
    let locked = SandboxStore::lock_existing(&dangling, &files(), None).unwrap();
    assert!(locked.remove().unwrap().is_none());
    fs::remove_dir(&sandboxes).unwrap();
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
    let malformed: [(Change, &str); 5] = [
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
        (|value| value["extra"] = json!(1), "is malformed"),
    ];
    for (change, problem) in malformed {
        rewrite(&bound, change);
        let error = Lease::of(&SandboxStore::open(&sandbox).unwrap()).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Io, "{error}");
        assert!(error.message().contains("is malformed"), "{error}");
        assert!(error.message().contains(problem), "{error}");
    }
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
            .args([
                "--exact",
                "host::template::store::leases::tests::transaction_role",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ROLE_STORE, store)
            .env(ROLE_ACTION, action)
            .env(ROLE_GENERATION, generation.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        if let Some(sandbox) = sandbox {
            command.env(ROLE_SANDBOX, sandbox);
        }
        let mut child = command.spawn().unwrap();
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

/// Tests of the records that a Windows file system without POSIX deletion keeps under their names
/// while other handles hold them, after a transaction in another process removed them.
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
}
