//! Finishing emulated capture output with genuine OS exit proof, without launching a VM.

#![cfg(any(target_os = "linux", windows))]

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::*;
use crate::host::config::{DiskCapture, NicConfig};
use crate::host::process::{BootId, Opened, Process, ProcessRef};
use crate::host::runtime::Hypervisor;
use crate::host::snapshot::Snapshot;
use crate::host::snapshot::tests::{ManifestSpec, SnapshotFixture, context};
use crate::host::vm::create_private_dir;

/// A producer kept alive through its stdin until its parent records its real identity.
#[test]
fn exit_helper() {
    if std::env::var_os("NVX_FINISH_CAPTURE_EXIT_HELPER").is_some() {
        std::io::stdin().read_to_end(&mut Vec::new()).unwrap();
    }
}

/// Reaps only the child that this test owns, including when a proof assertion fails.
struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
        }
        self.0.wait().unwrap();
    }
}

/// Waits for an owned emulated producer's actual exit, never forging a process reference.
fn exited() -> Exited {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "host::snapshot::capture::tests::exit_helper"])
        .env("NVX_FINISH_CAPTURE_EXIT_HELPER", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null());
    let mut child = OwnedChild(crate::host::tests::spawn_locked(&mut command).unwrap());
    let reference = ProcessRef {
        pid: child.0.id(),
        start_time: platform::process_start_time(child.0.id()).unwrap().unwrap(),
        boot: BootId::current().unwrap(),
    };
    let Opened::Running(process) = Process::open(&reference).unwrap() else {
        panic!("the owned producer is waiting for its parent");
    };
    assert!(process.wait(Instant::now()).unwrap().is_none());
    drop(child.0.stdin.take());
    let proof = process
        .wait(Instant::now() + Duration::from_secs(10))
        .unwrap()
        .expect("the owned producer exited");
    assert!(child.0.wait().unwrap().success());
    proof
}

/// A private VM directory containing OpenVMM's output but neither NVX metadata file.
struct Fixture {
    /// Keeps the sparse snapshot's private temporary directory alive.
    _snapshot: SnapshotFixture,
    /// The caller-owned VM directory.
    dir: VmDir,
    /// The checked inputs that the emulated capture used.
    context: CaptureContext,
}

impl Fixture {
    /// Uses the existing strict snapshot fixture and removes only its already-finished metadata.
    fn new() -> Self {
        let snapshot = SnapshotFixture::new();
        for name in [KEY_FILE, DESCRIPTION_FILE] {
            fs::remove_file(snapshot.dir.join(name)).unwrap();
        }
        let dir = VmDir::open(&snapshot.dir).unwrap();
        let context = context();
        let fixture = Self {
            _snapshot: snapshot,
            dir,
            context,
        };
        fixture.outcome("capture", "success", 0);
        fixture
    }

    /// Writes OpenVMM's capture outcome without any launch state.
    fn outcome(&self, operation: &str, category: &str, status: i64) {
        write_private(
            &self.dir.path().join(OUTCOME),
            &serde_json::to_vec(&json!({
                "outcome": {
                    "operation": operation,
                    "category": category,
                    "status_code": status,
                },
            }))
            .unwrap(),
        )
        .unwrap();
    }

    /// Returns one of OpenVMM's artifact paths.
    fn artifact(&self, name: &str) -> std::path::PathBuf {
        self.dir.path().join(OPENVMM_DIR).join(name)
    }

    /// Calls the public finalizer with the fixture's checked inputs.
    fn finish(&self, exited: &Exited) -> Result<CapturedSnapshot> {
        finish_capture(&self.dir, &self.context, exited)
    }

    /// Checks that validation has not written either metadata file.
    fn unfinished(&self) {
        assert!(!self.dir.path().join(KEY_FILE).exists());
        assert!(!self.dir.path().join(DESCRIPTION_FILE).exists());
    }
}

#[test]
fn finish_writes_matching_metadata_and_only_its_three_entries_are_published() {
    let mut fixture = Fixture::new();
    fixture.context.capture.invariants = super::super::Invariants::new()
        .with("isolation.test", "kept")
        .unwrap();
    let proof = exited();
    let outcome = fs::read(fixture.dir.path().join(OUTCOME)).unwrap();
    for (name, bytes) in [
        ("openvmm.log", b"VM log".as_slice()),
        ("caller.txt", b"caller data"),
    ] {
        write_private(&fixture.dir.path().join(name), bytes).unwrap();
    }
    let captured = fixture.finish(&proof).unwrap();
    let manifest = SnapshotManifest::read(&fixture.dir.path().join(OPENVMM_DIR)).unwrap();
    let key =
        SnapshotKey::from_bytes(&fs::read(fixture.dir.path().join(KEY_FILE)).unwrap()).unwrap();
    assert_eq!(key, captured.key);
    assert_eq!(key.time(), manifest.time());
    assert_eq!(key.runtime(), &fixture.context.runtime);
    assert_eq!(key.invariants(), &fixture.context.capture.invariants);
    key.check_manifest(&manifest).unwrap();
    let description: Value =
        serde_json::from_slice(&fs::read(fixture.dir.path().join(DESCRIPTION_FILE)).unwrap())
            .unwrap();
    assert_eq!(description["key"], key.identity().to_string());
    assert_eq!(
        description["runtime"],
        serde_json::to_value(key.runtime()).unwrap()
    );
    assert_eq!(
        description["capture"],
        serde_json::to_value(&fixture.context.capture).unwrap()
    );
    assert_eq!(description["memoryLen"], manifest.memory_len());
    assert_eq!(description["stateLen"], manifest.state_len());
    assert_eq!(
        description["time"]["captureTsc"],
        manifest.time().capture_tsc
    );
    assert_eq!(
        description["time"]["captureUtcNs"],
        manifest.time().capture_utc_ns
    );
    assert_eq!(
        description["time"]["bootId"],
        manifest.time().boot_id.to_string()
    );
    let published = fixture.dir.path().parent().unwrap().join("published");
    create_private_dir(&published).unwrap();
    for name in [KEY_FILE, DESCRIPTION_FILE, OPENVMM_DIR] {
        fs::rename(fixture.dir.path().join(name), published.join(name)).unwrap();
    }
    assert_eq!(Snapshot::open(&published).unwrap().key(), &captured.key);
    assert_eq!(fs::read_dir(&published).unwrap().count(), 3);
    assert_eq!(fs::read(fixture.dir.path().join(OUTCOME)).unwrap(), outcome);
    assert_eq!(
        fs::read(fixture.dir.path().join("openvmm.log")).unwrap(),
        b"VM log"
    );
    assert_eq!(
        fs::read(fixture.dir.path().join("caller.txt")).unwrap(),
        b"caller data"
    );
    assert!(Snapshot::open(fixture.dir.path()).is_err());
}

#[test]
fn finish_refuses_absent_and_unsuccessful_capture_outcomes() {
    let proof = exited();
    let fixture = Fixture::new();
    fs::remove_file(fixture.dir.path().join(OUTCOME)).unwrap();
    assert_eq!(
        fixture.finish(&proof).unwrap_err().kind(),
        ErrorKind::Exited
    );
    fixture.unfinished();
    for (operation, category, status) in [
        ("run", "success", 0),
        ("capture", "guest-exit", 3),
        ("capture", "vmm-failure", 1),
        ("capture", "success", 1),
        ("capture", "failure", 0),
    ] {
        fixture.outcome(operation, category, status);
        assert_eq!(
            fixture.finish(&proof).unwrap_err().kind(),
            ErrorKind::Exited
        );
        fixture.unfinished();
    }
}

#[test]
fn finish_uses_the_bounded_regular_outcome_reader() {
    let proof = exited();
    let fixture = Fixture::new();
    let path = fixture.dir.path().join(OUTCOME);
    for bytes in [
        b"not JSON".to_vec(),
        br#"{"outcome":{"category":"success","status_code":0}}"#.to_vec(),
        vec![b' '; MAX_REPORT_LEN as usize + 1],
    ] {
        write_private(&path, &bytes).unwrap();
        assert_eq!(fixture.finish(&proof).unwrap_err().kind(), ErrorKind::Io);
        fixture.unfinished();
    }
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert_eq!(fixture.finish(&proof).unwrap_err().kind(), ErrorKind::Io);
    fixture.unfinished();
}

#[test]
fn finish_refuses_every_missing_or_extra_openvmm_artifact() {
    let proof = exited();
    for missing in [
        None,
        Some(MANIFEST_NAME),
        Some("memory.bin"),
        Some("state.bin"),
    ] {
        let fixture = Fixture::new();
        match missing {
            Some(name) => fs::remove_file(fixture.artifact(name)).unwrap(),
            None => fs::rename(
                fixture.dir.path().join(OPENVMM_DIR),
                fixture.dir.path().join("saved-openvmm"),
            )
            .unwrap(),
        }
        assert_eq!(fixture.finish(&proof).unwrap_err().kind(), ErrorKind::Io);
        fixture.unfinished();
    }
    let fixture = Fixture::new();
    write_private(&fixture.artifact("extra"), b"unexpected").unwrap();
    assert_eq!(fixture.finish(&proof).unwrap_err().kind(), ErrorKind::Io);
    fixture.unfinished();
}

#[test]
fn finish_refuses_irregular_artifacts_and_both_recorded_length_mismatches() {
    let proof = exited();
    for name in SNAPSHOT_ARTIFACTS {
        let fixture = Fixture::new();
        fs::remove_file(fixture.artifact(name)).unwrap();
        fs::create_dir(fixture.artifact(name)).unwrap();
        assert_eq!(fixture.finish(&proof).unwrap_err().kind(), ErrorKind::Io);
        fixture.unfinished();
    }
    for name in ["memory.bin", "state.bin"] {
        for difference in [-1, 1] {
            let fixture = Fixture::new();
            let file = fs::OpenOptions::new()
                .write(true)
                .open(fixture.artifact(name))
                .unwrap();
            let length = file.metadata().unwrap().len();
            file.set_len(length.checked_add_signed(difference).unwrap())
                .unwrap();
            drop(file);
            assert_eq!(fixture.finish(&proof).unwrap_err().kind(), ErrorKind::Io);
            fixture.unfinished();
        }
    }
}

#[test]
fn finish_keeps_the_strict_manifest_checks() {
    let proof = exited();
    let fixture = Fixture::new();
    for bytes in [
        vec![0x80],
        vec![0; super::super::MAX_MANIFEST_LEN as usize + 1],
    ] {
        write_private(&fixture.artifact(MANIFEST_NAME), &bytes).unwrap();
        assert_eq!(fixture.finish(&proof).unwrap_err().kind(), ErrorKind::Io);
        fixture.unfinished();
    }
    let mut spec = ManifestSpec::template();
    spec.tier = "workload".to_owned();
    write_private(&fixture.artifact(MANIFEST_NAME), &spec.encode()).unwrap();
    assert_eq!(fixture.finish(&proof).unwrap_err().kind(), ErrorKind::Io);
    fixture.unfinished();
}

#[test]
fn finish_checks_the_context_against_the_effective_capture() {
    let proof = exited();
    let changes: &[fn(&mut CaptureContext)] = &[
        |context| context.hypervisor = Hypervisor::Mshv,
        |context| context.cpu_profile = Some("amd.zen4.v1".to_owned()),
        |context| context.capture.processors.capacity = 4,
        |context| context.capture.processors.boot = 1,
        |context| context.capture.memory.boot = 256,
        |context| context.capture.memory.capacity = 1024,
        |context| {
            let DiskCapture::Fresh { image, .. } = &mut context.capture.disks;
            image.length *= 2;
        },
        |context| {
            let DiskCapture::Fresh { scratch, .. } = &mut context.capture.disks;
            scratch.physical_block_size *= 2;
        },
        |context| {
            context.nic = Some(NicConfig {
                guest_network: crate::net::Ipv4Cidr::parse("10.0.0.2/24").unwrap(),
            });
        },
        |context| context.runtime.control_contract.clear(),
        |context| context.capture.memory.capacity = u64::MAX - 127,
    ];
    for change in changes {
        let mut fixture = Fixture::new();
        change(&mut fixture.context);
        assert!(fixture.finish(&proof).is_err());
        fixture.unfinished();
    }
}

#[test]
fn a_correctable_failure_and_a_partial_metadata_write_can_both_be_retried() {
    let proof = exited();
    let fixture = Fixture::new();
    let state = fixture.artifact("state.bin");
    let length = fs::metadata(&state).unwrap().len();
    fs::OpenOptions::new()
        .write(true)
        .open(&state)
        .unwrap()
        .set_len(length - 1)
        .unwrap();
    assert!(fixture.finish(&proof).is_err());
    fixture.unfinished();
    fs::OpenOptions::new()
        .write(true)
        .open(&state)
        .unwrap()
        .set_len(length)
        .unwrap();
    let description = fixture.dir.path().join(DESCRIPTION_FILE);
    fs::create_dir(&description).unwrap();
    assert!(fixture.finish(&proof).is_err());
    assert!(fixture.dir.path().join(KEY_FILE).is_file());
    assert!(description.is_dir());
    fs::remove_dir(&description).unwrap();
    let captured = fixture.finish(&proof).unwrap();
    assert_eq!(fixture.finish(&proof).unwrap().key, captured.key);
    assert!(Snapshot::open(fixture.dir.path()).is_err());
    let published = fixture.dir.path().parent().unwrap().join("published");
    create_private_dir(&published).unwrap();
    for name in [KEY_FILE, DESCRIPTION_FILE, OPENVMM_DIR] {
        fs::rename(fixture.dir.path().join(name), published.join(name)).unwrap();
    }
    assert_eq!(Snapshot::open(&published).unwrap().key(), &captured.key);
    let names = fs::read_dir(fixture.dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(names, [std::ffi::OsString::from(OUTCOME)]);
}

#[cfg(target_os = "linux")]
#[test]
fn finish_refuses_shared_snapshot_entries_links_fifos_and_sockets() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::os::unix::net::UnixListener;

    let proof = exited();
    for name in SNAPSHOT_ARTIFACTS {
        let fixture = Fixture::new();
        let path = fixture.artifact(name);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(fixture.finish(&proof).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let saved = fixture.dir.path().join("saved");
        fs::rename(&path, &saved).unwrap();
        symlink(&saved, &path).unwrap();
        assert!(fixture.finish(&proof).is_err());
        fs::remove_file(&path).unwrap();
        nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRUSR).unwrap();
        assert!(fixture.finish(&proof).is_err());
        fs::remove_file(&path).unwrap();
        let _socket = UnixListener::bind(&path).unwrap();
        assert!(fixture.finish(&proof).is_err());
        fixture.unfinished();
    }
    let fixture = Fixture::new();
    fs::set_permissions(
        fixture.dir.path().join(OPENVMM_DIR),
        fs::Permissions::from_mode(0o750),
    )
    .unwrap();
    assert!(fixture.finish(&proof).is_err());
    fixture.unfinished();
}

#[cfg(windows)]
#[test]
fn finish_refuses_shared_snapshot_acls_and_reparse_artifacts() {
    use crate::host::platform::tests::{set_access, user_sid};

    let proof = exited();
    let shared = format!("D:P(A;;FA;;;{})(A;;FR;;;WD)", user_sid());
    for name in SNAPSHOT_ARTIFACTS {
        let fixture = Fixture::new();
        let path = fixture.artifact(name);
        set_access(&path, &shared).unwrap();
        assert!(fixture.finish(&proof).is_err());
        fs::remove_file(&path).unwrap();
        crate::host::tests::junction(&path, fixture.dir.path());
        assert!(fixture.finish(&proof).is_err());
        fixture.unfinished();
        fs::remove_dir(&path).unwrap();
    }
    let fixture = Fixture::new();
    set_access(&fixture.dir.path().join(OPENVMM_DIR), &shared).unwrap();
    assert!(fixture.finish(&proof).is_err());
    fixture.unfinished();
}
