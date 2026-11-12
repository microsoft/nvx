#![cfg(any(target_os = "linux", windows))]

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use super::*;
use crate::host::config::NicConfig;
use crate::host::platform;
use crate::host::process::{BootId, Opened, ProcessRef};
use crate::host::snapshot::tests::{ManifestSpec, SnapshotFixture, context};
use crate::host::snapshot::{MANIFEST_NAME, Snapshot};
use crate::host::spawn::{self, VmmCommand};
use crate::protocol::v2::{self, Frame};
use crate::protocol::{
    CAPABILITY_LEN, OUTER_DATA, OUTER_HEADER_LEN, OUTER_HOST_ATTACH, OUTER_READY,
    decode_outer_header, encode_outer,
};

const HELPER: &str = "host::cli::snapshots::capture::tests::capture_helper";
const COMMAND_HELPER: &str = "host::cli::snapshots::capture::tests::command_helper";
/// Names the VM directory that a fake VMM started before its capture enters once its capability
/// arrives.
const STARTED_VM_DIR: &str = "NVX_CAPTURE_STARTED_VM_DIR";

type Inventory = Vec<(PathBuf, (u64, u128), u64, std::time::SystemTime)>;

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
        }
    }

    fn options(&self, name: &str) -> Options {
        options(self.root.path(), name)
    }

    fn capture(&self, name: &str, behavior: &str, timeout: u32) -> Result {
        let mut options = self.options(name);
        options.timeout = timeout;
        capture(&options, &runtime(self.root.path()), |prepared| {
            Ok(fake_spawn(self.root.path(), prepared, behavior))
        })
    }

    /// Captures with a fake VMM that starts before the capture and waits for the capability that
    /// its launch hands over, so that the capture's timeout, which bounds launch too, never runs
    /// while the VMM's process starts. The returned guard ends the fake only once dropped, so that
    /// the caller can check first that the capture stopped it.
    fn capture_started(&self, name: &str, behavior: &str, timeout: u32) -> (Result, OwnedCommand) {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", HELPER, "--ignored", "--nocapture"])
            .env(STARTED_VM_DIR, self.vm_dir())
            .current_dir(self.root.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut vmm = OwnedCommand(crate::host::tests::spawn_locked(&mut command).unwrap());
        let reference = ProcessRef {
            pid: vmm.0.id(),
            start_time: platform::process_start_time(vmm.0.id()).unwrap().unwrap(),
            boot: BootId::current().unwrap(),
        };
        record(self.root.path(), &reference);
        let Opened::Running(process) = Process::open(&reference).unwrap() else {
            panic!("the fake VMM exited before its capture");
        };
        wait_file(&self.root.path().join("started"));
        let mut stdin = vmm.0.stdin.take().unwrap();
        let mut options = self.options(name);
        options.timeout = timeout;
        let result = capture(&options, &runtime(self.root.path()), |prepared| {
            write_recipe(self.root.path(), &prepared, behavior);
            // As the crate's spawn of a VMM does, this hands the VMM its capability on its standard
            // input.
            stdin.write_all(prepared.capability().as_bytes()).unwrap();
            drop(stdin);
            Ok(process)
        });
        (result, vmm)
    }

    fn store(&self) -> Store {
        Store::create(&self.root.path().join("store")).unwrap()
    }

    fn vm_dir(&self) -> PathBuf {
        self.root.path().join("store").join("capture")
    }

    fn references(&self) -> Vec<ProcessRef> {
        match fs::read(self.root.path().join("processes.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("cannot read owned process receipts: {error}"),
        }
    }

    fn existing(&self) {
        let store = self.store();
        let held = store
            .lock(&name("existing"), Deadline::after(10.0))
            .unwrap();
        let staged = held.stage().unwrap();
        let fixture = SnapshotFixture::new();
        for entry in [KEY_FILE, DESCRIPTION_FILE, OPENVMM_DIR] {
            fs::rename(fixture.dir.join(entry), staged.dir().join(entry)).unwrap();
        }
        held.publish(staged).unwrap();
    }

    fn inventory(&self) -> Inventory {
        let dir = self
            .root
            .path()
            .join("store")
            .join("snapshots")
            .join("6578697374696e67");
        [
            KEY_FILE,
            DESCRIPTION_FILE,
            "openvmm/manifest.bin",
            "openvmm/memory.bin",
            "openvmm/state.bin",
        ]
        .into_iter()
        .map(|entry| {
            let path = dir.join(entry);
            let metadata = fs::metadata(&path).unwrap();
            let id = platform::file_id(&path).unwrap();
            (path, id, metadata.len(), metadata.modified().unwrap())
        })
        .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for reference in self.references() {
            if let Opened::Running(process) = Process::open(&reference).unwrap() {
                if process.wait(Instant::now()).unwrap().is_none() {
                    process.kill().unwrap();
                }
                assert!(
                    process
                        .wait(Instant::now() + KILL_TIMEOUT)
                        .unwrap()
                        .is_some()
                );
            }
        }
    }
}

fn name(text: &str) -> Name {
    Name::try_from(text.to_owned()).unwrap()
}

fn options(root: &Path, snapshot: &str) -> Options {
    let arguments = [
        snapshot.to_owned(),
        "--store-dir".to_owned(),
        root.join("store").to_str().unwrap().to_owned(),
        "--processors".to_owned(),
        "2".to_owned(),
        "--memory-mib".to_owned(),
        "512".to_owned(),
        "--memory-capacity-mib".to_owned(),
        "2048".to_owned(),
    ];
    let (options, extras) = parse(&arguments).unwrap();
    assert!(extras.is_empty());
    options
}

fn runtime(root: &Path) -> Runtime {
    Runtime {
        openvmm: std::env::current_exe().unwrap(),
        kernel: root.join("fake-kernel"),
        initramfs: root.join("fake-initramfs"),
        identity: context().runtime,
        hypervisor: if cfg!(windows) {
            crate::Hypervisor::Whp
        } else {
            crate::Hypervisor::Kvm
        },
        cpu_profile: None,
        windows_breakaway: false,
        options: LaunchOptions::default(),
    }
}

/// Checks the durable barrier that a spawn follows, and writes the fake VMM's recipe.
fn write_recipe(root: &Path, prepared: &Prepared, behavior: &str) {
    let dir = prepared.outputs().log.parent().unwrap();
    let journal: Value =
        serde_json::from_slice(&fs::read(dir.join("launch.json")).unwrap()).unwrap();
    assert!(
        journal["process"].is_null(),
        "spawn must follow the durable prepared barrier"
    );
    assert_eq!(journal["log"], prepared.outputs().log.to_str().unwrap());
    assert_eq!(
        journal["outcome"],
        prepared.outputs().outcome.to_str().unwrap()
    );
    assert_eq!(journal["endpoint"], prepared.outputs().endpoint);
    assert_eq!(
        fs::read(dir.join("control.capability")).unwrap(),
        prepared.capability().as_bytes()
    );
    assert_eq!(
        journal["capability"],
        json!(prepared.capability().as_bytes())
    );
    assert!(journal["console"].is_null());
    assert!(!dir.join("runtime.json").exists());
    let command_line = prepared
        .arguments()
        .windows(2)
        .find(|pair| pair[0] == "--cmdline")
        .unwrap()[1]
        .to_str()
        .unwrap();
    assert_eq!(
        command_line
            .split_whitespace()
            .filter(|token| *token == "nvx_sandbox=1")
            .count(),
        1
    );
    let context = prepared.capture_context().unwrap();
    let DiskCapture::Fresh { image, scratch } = context.capture.disks;
    // The fake's manifest has a NIC, at 10.0.0.2/24, only for a VM that has one.
    let mut manifest = match context.nic {
        Some(_) => ManifestSpec::with_network(),
        None => ManifestSpec::template(),
    };
    let contract = manifest.contract();
    contract.source_hypervisor = runtime(root).hypervisor.to_string();
    contract.blocks[0].physical_block_size = image.physical_block_size;
    contract.blocks[1].physical_block_size = scratch.physical_block_size;
    let recipe = json!({
        "root": root, "endpoint": prepared.outputs().endpoint, "behavior": behavior,
        "manifest": manifest.encode(), "memory": manifest.memory_size, "state": manifest.state_size,
    });
    fs::write(
        root.join("helper.json"),
        serde_json::to_vec(&recipe).unwrap(),
    )
    .unwrap();
}

fn fake_spawn(root: &Path, prepared: Prepared, behavior: &str) -> Process {
    write_recipe(root, &prepared, behavior);
    let command = VmmCommand {
        program: std::env::current_exe().unwrap(),
        arguments: ["--exact", HELPER, "--ignored", "--nocapture"]
            .map(Into::into)
            .to_vec(),
        working_dir: prepared.outputs().log.parent().unwrap().to_path_buf(),
        breakaway_from_job: false,
        inherit: Vec::new(),
    };
    let child = spawn::spawn_vm_with_mode(
        &command,
        prepared.capability().as_bytes(),
        prepared.log().try_clone().unwrap(),
        false,
    )
    .unwrap();
    let process = child.process().unwrap();
    record(root, process.reference());
    child.detach();
    drop(prepared);
    process
}

/// Records an owned fake VMM, which the fixture ends if a test leaves it running.
fn record(root: &Path, reference: &ProcessRef) {
    let path = root.join("processes.json");
    let mut references: Vec<ProcessRef> = if path.exists() {
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap()
    } else {
        Vec::new()
    };
    references.push(reference.clone());
    fs::write(path, serde_json::to_vec(&references).unwrap()).unwrap();
}

fn wait_file(path: &Path) {
    let until = Instant::now() + Duration::from_secs(20);
    while !path.exists() {
        assert!(Instant::now() < until, "missing receipt {}", path.display());
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn capture_parser_refuses_bad_names_and_invalid_options() {
    for arguments in [
        vec!["../escape", "--store-dir", "store"],
        vec!["name"],
        vec!["name", "--store-dir", "store", "--timeout", "0"],
        vec!["name", "--store-dir", "store", "--processors", "3"],
    ] {
        assert!(
            parse(
                &arguments
                    .iter()
                    .map(|value| (*value).to_owned())
                    .collect::<Vec<_>>()
            )
            .is_err()
        );
    }
    assert!(matches!(parse(&["--help".to_owned()]), Err(Exit::Help(_))));
    CAPTURE_SYNTAX.assert_lists(&OPTIONS);
}

#[test]
fn the_capture_parser_takes_for_net_only_an_address_that_openvmm_accepts() {
    let parsed = |extra: &[&str]| {
        let arguments: Vec<String> = ["name", "--store-dir", "store"]
            .iter()
            .chain(extra)
            .map(|value| (*value).to_owned())
            .collect();
        parse(&arguments)
    };
    assert_eq!(parsed(&[]).unwrap().0.net, None);
    assert_eq!(
        parsed(&["--net", "192.0.2.2/24"]).unwrap().0.net,
        Some(Ipv4Cidr::parse("192.0.2.2/24").unwrap())
    );
    // The network's own, broadcast and gateway addresses, an address without a prefix, a prefix
    // longer than /30, and IPv6.
    for refused in [
        "192.0.2.0/24",
        "192.0.2.255/24",
        "192.0.2.1/24",
        "192.0.2.2",
        "192.0.2.2/31",
        "fd00::2/120",
        "",
    ] {
        assert!(
            matches!(
                parsed(&["--net", refused]),
                Err(Exit::Usage {
                    code: ErrorCode::ArgumentInvalid,
                    ..
                })
            ),
            "{refused:?}"
        );
    }
}

#[test]
fn a_capture_with_net_gives_its_vm_a_deny_all_nic_which_its_snapshot_keeps() {
    let fixture = Fixture::new();
    let store = fixture.root.path().join("store");
    let arguments = [
        "networked",
        "--store-dir",
        store.to_str().unwrap(),
        "--processors",
        "2",
        "--memory-mib",
        "512",
        "--memory-capacity-mib",
        "2048",
        "--net",
        "10.0.0.2/24",
        "--timeout",
        "20",
    ]
    .map(str::to_owned);
    let (networked, extras) = parse(&arguments).unwrap();
    assert!(extras.is_empty());
    let address = Ipv4Cidr::parse("10.0.0.2/24").unwrap();
    let mut launched = Vec::new();
    capture(&networked, &runtime(fixture.root.path()), |prepared| {
        assert_eq!(
            prepared.capture_context().unwrap().nic,
            Some(NicConfig {
                guest_network: address
            })
        );
        launched = prepared.arguments().to_vec();
        Ok(fake_spawn(fixture.root.path(), prepared, "publish"))
    })
    .unwrap();
    // OpenVMM attaches the NIC under the complete deny-all policy alone: no egress, ingress,
    // host loopback, forward, proxy or DNS server.
    let deny_all = [
        "--net",
        "10.0.0.2/24",
        "--network-profile",
        "portable",
        "--network-egress",
        "deny",
        "--network-ingress",
        "deny",
        "--host-loopback",
        "deny",
    ]
    .map(OsString::from);
    assert_eq!(
        launched
            .windows(deny_all.len())
            .filter(|window| *window == deny_all)
            .count(),
        1,
        "{launched:?}"
    );
    for option in [
        "--network-egress-allow",
        "--network-egress-deny",
        "--host-loopback-forward",
        "--network-proxy",
    ] {
        assert!(
            !launched.iter().any(|argument| argument == option),
            "{option}"
        );
    }
    let store = fixture.store();
    let held = store
        .lock(&name("networked"), Deadline::after(10.0))
        .unwrap();
    assert_eq!(
        held.open().unwrap().nic(),
        Some(NicConfig {
            guest_network: address
        })
    );
    let description: Value =
        serde_json::from_slice(&fs::read(held.dir.join(DESCRIPTION_FILE)).unwrap()).unwrap();
    assert_eq!(
        description["nic"],
        json!({ "guest_network": "10.0.0.2/24" })
    );
    drop(held);
    assert!(!fixture.vm_dir().exists());
    // Without --net, a capture's VM has no NIC, and neither has its snapshot.
    capture(
        &fixture.options("plain"),
        &runtime(fixture.root.path()),
        |prepared| {
            assert_eq!(prepared.capture_context().unwrap().nic, None);
            assert!(
                !prepared
                    .arguments()
                    .iter()
                    .any(|argument| argument.to_str().unwrap().starts_with("--net"))
            );
            Ok(fake_spawn(fixture.root.path(), prepared, "publish"))
        },
    )
    .unwrap();
    let held = store.lock(&name("plain"), Deadline::after(10.0)).unwrap();
    assert_eq!(held.open().unwrap().nic(), None);
}

#[test]
fn capture_publishes_only_three_strict_entries_after_its_own_exit_and_removes_its_vm() {
    let fixture = Fixture::new();
    fixture.capture("first", "publish", 20).unwrap();
    let store = fixture.store();
    let held = store.lock(&name("first"), Deadline::after(10.0)).unwrap();
    let snapshot = held.open().unwrap();
    assert_eq!(
        snapshot.key().runtime(),
        &runtime(fixture.root.path()).identity
    );
    let entries: Vec<_> = fs::read_dir(&held.dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries.len(), 3);
    for entry in [KEY_FILE, DESCRIPTION_FILE, OPENVMM_DIR] {
        assert!(entries.iter().any(|name| name == entry));
    }
    assert!(Snapshot::open(&held.dir).is_ok());
    assert!(!fixture.vm_dir().exists());
    let receipt: Value =
        serde_json::from_slice(&fs::read(fixture.root.path().join("ack.json")).unwrap()).unwrap();
    assert_eq!(receipt["capture"], true);
    assert_eq!(receipt["finalizedWhileAlive"], false);
    assert_eq!(receipt["pid"], fixture.references()[0].pid);
    assert!(matches!(
        Process::open(&fixture.references()[0]).unwrap(),
        Opened::Exited(_)
    ));
}

#[test]
fn duplicate_names_and_a_failed_new_capture_never_change_an_existing_snapshot() {
    let fixture = Fixture::new();
    fixture.existing();
    let before = fixture.inventory();
    let options = fixture.options("existing");
    assert!(
        capture(&options, &runtime(fixture.root.path()), |_| panic!(
            "duplicate must not spawn"
        ))
        .is_err()
    );
    assert_eq!(fixture.inventory(), before);
    assert!(!fixture.vm_dir().exists());
    assert_eq!(
        fixture.capture("failed", "fail", 20).unwrap_err().code(),
        Some(ErrorCode::Exited)
    );
    let store = fixture.store();
    assert_eq!(fs::read_dir(store.entries.dir()).unwrap().count(), 1);
    assert_eq!(fixture.inventory(), before);
    assert!(
        store
            .lock(&name("existing"), Deadline::after(10.0))
            .unwrap()
            .open()
            .is_ok()
    );
    assert!(!fixture.vm_dir().join(KEY_FILE).exists());
    assert!(
        capture(&options, &runtime(fixture.root.path()), |_| panic!(
            "duplicate must not spawn"
        ))
        .is_err()
    );
    assert!(!fixture.vm_dir().exists());
    assert_eq!(fixture.inventory(), before);
}

#[test]
fn a_capture_ack_and_successful_output_without_timely_exit_are_stopped_but_never_finished() {
    let fixture = Fixture::new();
    // The timeout bounds launch too: the fake VMM starts before it, and the timeout leaves the
    // handshake of two processes room on a loaded host, so that only the exit wait times out.
    let (result, _vmm) = fixture.capture_started("late", "hang", 5);
    let error = result.unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::TimedOut), "{error:?}");
    assert_eq!(
        error.message(),
        Some("capture VM did not exit before its deadline"),
        "only the wait for the acknowledged capture's exit may time out"
    );
    assert!(fixture.root.path().join("ack.json").exists());
    assert!(fixture.vm_dir().join(OPENVMM_DIR).exists());
    assert!(fixture.vm_dir().join("outcome.json").exists());
    assert!(!fixture.vm_dir().join(KEY_FILE).exists());
    assert!(!fixture.vm_dir().join(DESCRIPTION_FILE).exists());
    assert!(fixture.vm_dir().join("launch.json").exists());
    assert!(matches!(
        Process::open(&fixture.references()[0]).unwrap(),
        Opened::Exited(_)
    ));
    assert_eq!(
        fs::read_dir(fixture.store().entries.dir()).unwrap().count(),
        0
    );
    fixture.existing();
    let options = fixture.options("existing");
    assert!(
        capture(&options, &runtime(fixture.root.path()), |_| panic!(
            "duplicate must not spawn"
        ))
        .is_err()
    );
    assert!(
        !fixture.vm_dir().exists(),
        "later exit must only permit recovery, not finishing"
    );
    assert_eq!(
        fs::read_dir(fixture.store().entries.dir()).unwrap().count(),
        1
    );
}

#[test]
fn a_capture_rejection_stops_its_verified_vm_without_finishing_or_publishing() {
    let fixture = Fixture::new();
    let error = fixture.capture("refused", "refuse", 20).unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::Protocol), "{error:?}");
    assert!(matches!(
        Process::open(&fixture.references()[0]).unwrap(),
        Opened::Exited(_)
    ));
    assert!(fixture.vm_dir().join("launch.json").exists());
    assert!(!fixture.vm_dir().join(KEY_FILE).exists());
    assert!(!fixture.vm_dir().join(DESCRIPTION_FILE).exists());
    assert_eq!(
        fs::read_dir(fixture.store().entries.dir()).unwrap().count(),
        0
    );
}

#[test]
fn unjournaled_user_content_is_preserved_before_any_capture_slot_mutation() {
    for entry in [
        "personal.txt",
        KEY_FILE,
        OPENVMM_DIR,
        ".openvmm-personal",
        ".tmpAb3xY9",
        ".outcome.json.7.tmp",
    ] {
        let fixture = Fixture::new();
        let store = fixture.store();
        StateDir::create(&fixture.vm_dir()).unwrap();
        let path = fixture.vm_dir().join(entry);
        let data = if entry == OPENVMM_DIR {
            StateDir::create(&path).unwrap();
            path.join("personal.txt")
        } else {
            path
        };
        fs::write(&data, b"preserve user content").unwrap();
        let missing_artifacts = fixture.root.path().join("missing-artifacts");
        let error = run(missing_artifacts.to_str(), fixture.options("first")).unwrap_err();
        assert!(error.message().unwrap().contains("unowned capture content"));
        assert_eq!(fs::read(data).unwrap(), b"preserve user content");
        assert!(!fixture.vm_dir().join("sandbox.lock").exists());
        assert!(!fixture.vm_dir().join("launch.json").exists());
        assert_eq!(fs::read_dir(store.entries.dir()).unwrap().count(), 0);
    }
}

struct OwnedCommand(Child);

impl Drop for OwnedCommand {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
        }
        self.0.wait().unwrap();
    }
}

fn interrupted(fixture: &Fixture) {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", COMMAND_HELPER, "--ignored", "--nocapture"])
        .env("NVX_CAPTURE_COMMAND_ROOT", fixture.root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = OwnedCommand(crate::host::tests::spawn_locked(&mut command).unwrap());
    wait_file(&fixture.root.path().join("ack.json"));
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    assert!(fixture.vm_dir().join("launch.json").exists());
    assert!(!fixture.vm_dir().join(KEY_FILE).exists());
    assert!(matches!(
        Process::open(&fixture.references()[0]).unwrap(),
        Opened::Running(_)
    ));
}

#[test]
fn the_next_command_ends_an_interrupted_capture_and_removes_its_directory_without_publishing() {
    let fixture = Fixture::new();
    fixture.existing();
    let before = fixture.inventory();
    interrupted(&fixture);
    let missing_artifacts = fixture.root.path().join("missing-artifacts");
    let error = run(missing_artifacts.to_str(), fixture.options("existing")).unwrap_err();
    let opened = Process::open(&fixture.references()[0]).unwrap();
    assert!(
        matches!(&opened, Opened::Exited(_)),
        "next-command recovery returned {error:?}; process is {opened:?}"
    );
    assert!(!fixture.vm_dir().exists());
    assert_eq!(fixture.inventory(), before);
    assert_eq!(
        fs::read_dir(fixture.store().entries.dir()).unwrap().count(),
        1
    );
}

#[test]
fn interrupted_atomic_outputs_are_recovered_only_after_their_vm_exits() {
    for entry in [
        ".launch.json.0.tmp",
        ".control.capability.18446744073709551615.tmp",
        ".outcome.json.7.tmp",
        ".key.bin.0.tmp",
        ".snapshot.json.1.tmp",
        ".tmpAb3xY9",
    ] {
        let fixture = Fixture::new();
        interrupted(&fixture);
        fs::write(fixture.vm_dir().join(entry), b"interrupted atomic output").unwrap();
        let missing_artifacts = fixture.root.path().join("missing-artifacts");
        let error = run(missing_artifacts.to_str(), fixture.options("next")).unwrap_err();
        let opened = Process::open(&fixture.references()[0]).unwrap();
        assert!(
            matches!(&opened, Opened::Exited(_)),
            "{entry}: recovery returned {error:?}; process is {opened:?}"
        );
        assert!(!fixture.vm_dir().exists(), "{entry}: {error:?}");
        assert_eq!(
            fs::read_dir(fixture.store().entries.dir()).unwrap().count(),
            0
        );
    }
}

#[test]
fn journaled_unknown_and_near_miss_names_are_preserved_without_recovery_or_publication() {
    for entry in [
        ".tmp12345",
        ".tmp1234567",
        ".tmpabcd\u{e9}",
        ".outcome.json..tmp",
        ".outcome.json.-1.tmp",
        ".outcome.json.+1.tmp",
        ".outcome.json.01.tmp",
        ".outcome.json.18446744073709551616.tmp",
        ".config.json.1.tmp",
        ".runtime.json.1.tmp",
        ".personal.txt.1.tmp",
        "config.json",
    ] {
        let fixture = Fixture::new();
        interrupted(&fixture);
        let journal = fs::read(fixture.vm_dir().join("launch.json")).unwrap();
        let capability = fs::read(fixture.vm_dir().join("control.capability")).unwrap();
        let path = fixture.vm_dir().join(entry);
        fs::write(&path, b"preserve user content").unwrap();
        let missing_artifacts = fixture.root.path().join("missing-artifacts");
        let error = run(missing_artifacts.to_str(), fixture.options("next")).unwrap_err();
        assert!(
            error.message().unwrap().contains("unowned capture content"),
            "{entry}: {error:?}"
        );
        assert_eq!(fs::read(path).unwrap(), b"preserve user content");
        assert_eq!(
            fs::read(fixture.vm_dir().join("launch.json")).unwrap(),
            journal
        );
        assert_eq!(
            fs::read(fixture.vm_dir().join("control.capability")).unwrap(),
            capability
        );
        assert!(matches!(
            Process::open(&fixture.references()[0]).unwrap(),
            Opened::Running(_)
        ));
        assert_eq!(
            fs::read_dir(fixture.store().entries.dir()).unwrap().count(),
            0
        );
    }
}

#[test]
fn indeterminate_recovery_retains_the_live_vm_its_capability_and_journal() {
    let fixture = Fixture::new();
    interrupted(&fixture);
    let journal_path = fixture.vm_dir().join("launch.json");
    let mut journal: Value = serde_json::from_slice(&fs::read(&journal_path).unwrap()).unwrap();
    journal["process"]["pid"] = json!(0);
    fs::write(&journal_path, serde_json::to_vec(&journal).unwrap()).unwrap();
    let journal = fs::read(&journal_path).unwrap();
    let capability = fs::read(fixture.vm_dir().join("control.capability")).unwrap();
    fs::write(fixture.vm_dir().join(".tmpAb3xY9"), b"owned report staging").unwrap();
    let options = fixture.options("next");
    let error = capture(&options, &runtime(fixture.root.path()), |_| {
        panic!("recovery must refuse")
    })
    .unwrap_err();
    assert!(
        error
            .message()
            .unwrap()
            .contains("cannot verify the recorded VM process"),
        "{error:?}"
    );
    assert_eq!(fs::read(journal_path).unwrap(), journal);
    assert_eq!(
        fs::read(fixture.vm_dir().join("control.capability")).unwrap(),
        capability
    );
    assert!(matches!(
        Process::open(&fixture.references()[0]).unwrap(),
        Opened::Running(_)
    ));
    assert_eq!(
        fs::read_dir(fixture.store().entries.dir()).unwrap().count(),
        0
    );
}

#[test]
#[ignore = "owned subprocess of interrupted-capture command tests"]
fn command_helper() {
    let root = PathBuf::from(std::env::var_os("NVX_CAPTURE_COMMAND_ROOT").unwrap());
    let result = capture(&options(&root, "abandoned"), &runtime(&root), |prepared| {
        Ok(fake_spawn(&root, prepared, "hang"))
    });
    panic!("the test must interrupt this capture: {result:?}");
}

fn read_record(stream: &mut (impl Read + Write)) -> (u8, Vec<u8>) {
    let mut header = [0; OUTER_HEADER_LEN];
    stream.read_exact(&mut header).unwrap();
    let header = decode_outer_header(&header).unwrap();
    assert!(header.length <= crate::protocol::OUTER_MAX_PAYLOAD);
    let mut payload = vec![0; header.length];
    stream.read_exact(&mut payload).unwrap();
    (header.record_type, payload)
}

fn respond(stream: &mut (impl Read + Write), capability: &[u8], refused: bool) {
    let (kind, payload) = read_record(stream);
    assert_eq!(kind, OUTER_HOST_ATTACH);
    assert_eq!(payload, capability);
    stream
        .write_all(&encode_outer(OUTER_READY, &[7; 16], 1, 0, &[]).unwrap())
        .unwrap();
    let (kind, payload) = read_record(stream);
    assert_eq!(kind, OUTER_DATA);
    // The capture request of NVXC version 2, with its flag that keeps the executions.
    let (request_id, frame) = Frame::decode(&payload).unwrap();
    assert_eq!(frame, Frame::Capture(&[0]));
    let reply = if refused {
        Frame::Error {
            status: 22,
            category: b"capture-failed",
        }
    } else {
        Frame::Ready {
            version: v2::VERSION,
            activation: &[],
        }
    };
    let reply = reply.encode(request_id).unwrap();
    stream
        .write_all(&encode_outer(OUTER_DATA, &[7; 16], 1, 1, &reply).unwrap())
        .unwrap();
}

#[test]
#[ignore = "authenticated fake VMM subprocess; not real-VM capture proof"]
fn capture_helper() {
    // Started before its capture, the fake reports that it runs, and enters its VM directory,
    // as a spawned VMM starts in it, once its capability arrives.
    let started = std::env::var_os(STARTED_VM_DIR);
    if started.is_some() {
        fs::write("started", b"").unwrap();
    }
    let mut capability = Vec::new();
    std::io::stdin().read_to_end(&mut capability).unwrap();
    if let Some(dir) = started {
        std::env::set_current_dir(dir).unwrap();
    }
    assert_eq!(capability.len(), CAPABILITY_LEN);
    assert_eq!(fs::read("control.capability").unwrap(), capability);
    let directory = std::env::current_dir().unwrap();
    let recipe_path = directory
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("helper.json");
    let recipe: Value = serde_json::from_slice(&fs::read(recipe_path).unwrap()).unwrap();
    let endpoint = recipe["endpoint"].as_str().unwrap();
    let refused = recipe["behavior"] == "refuse";
    #[cfg(target_os = "linux")]
    {
        let server = std::os::unix::net::UnixListener::bind(endpoint).unwrap();
        let (mut stream, _) = server.accept().unwrap();
        respond(&mut stream, &capability, refused);
    }
    #[cfg(windows)]
    {
        let mut server = platform::tests::control_server(endpoint);
        platform::tests::accept_control(&server);
        respond(&mut server, &capability, refused);
    }
    let journal: Value = serde_json::from_slice(&fs::read("launch.json").unwrap()).unwrap();
    assert_eq!(journal["process"]["pid"], std::process::id());
    assert_eq!(journal["endpoint"], endpoint);
    let root = PathBuf::from(recipe["root"].as_str().unwrap());
    fs::write(root.join("ack.json"), serde_json::to_vec(&json!({
        "capture": true, "pid": std::process::id(),
        "finalizedWhileAlive": Path::new(KEY_FILE).exists() || Path::new(DESCRIPTION_FILE).exists(),
    })).unwrap()).unwrap();
    vm::create_private_dir(Path::new(OPENVMM_DIR)).unwrap();
    let manifest: Vec<u8> = serde_json::from_value(recipe["manifest"].clone()).unwrap();
    crate::host::platform::write_private(&Path::new(OPENVMM_DIR).join(MANIFEST_NAME), &manifest)
        .unwrap();
    if refused {
        thread::sleep(Duration::from_secs(120));
        return;
    }
    for (entry, field) in [("memory.bin", "memory"), ("state.bin", "state")] {
        let file = crate::host::platform::create_new(&Path::new(OPENVMM_DIR).join(entry)).unwrap();
        platform::make_sparse(&file).unwrap();
        file.set_len(recipe[field].as_u64().unwrap()).unwrap();
    }
    let failed = recipe["behavior"] == "fail";
    crate::host::platform::write_private(Path::new("outcome.json"), &serde_json::to_vec(&json!({
        "outcome": { "operation": "capture", "category": if failed { "failure" } else { "success" },
            "status_code": if failed { 1 } else { 0 } },
    })).unwrap()).unwrap();
    if recipe["behavior"] == "hang" {
        thread::sleep(Duration::from_secs(120));
    } else {
        thread::sleep(Duration::from_millis(100));
        assert!(
            !Path::new(KEY_FILE).exists(),
            "ACK and completed output are not exit proof"
        );
        assert!(!Path::new(DESCRIPTION_FILE).exists());
    }
}
