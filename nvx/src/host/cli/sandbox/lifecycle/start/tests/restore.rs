use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Value, json};

use super::super::*;
use crate::guest_restore::{
    GuestRestoreReady, GuestRestoreRecord, Image, Network, Publication, Upper,
};
use crate::host::artifacts::{
    ArtifactDir, Guest, INITRAMFS_PROVENANCE_NAME, KERNEL_PROVENANCE_NAME, OPENVMM_PROVENANCE_NAME,
};
use crate::host::cli::hex;
use crate::host::cli::run;
use crate::host::cli::sandbox::lifecycle::{ProvisionConfig, deprovision};
use crate::host::cli::sandbox::sandbox_network;
use crate::host::cli::sandbox::state::{RecordedDistro, serialize_launch, write_json};
use crate::host::cli::snapshots::{self, Name};
use crate::host::cli::store::StateDir;
use crate::host::filesystem::{Bind, ExportChild, ExportPlan, Owner};
use crate::host::network::{Access, NetworkPolicy};
use crate::host::paths::resolve;
use crate::host::process::{Opened, Process, ProcessRef};
use crate::host::sandbox::distro::DistroBinding;
use crate::host::sandbox::layer::{LayerRole, SandboxLayer};
use crate::host::sandbox::mount::{plan_mounts, validate_mounts};
use crate::host::sandbox::options::sandbox_mounts;
use crate::host::sandbox::{DEFAULT_HOSTNAME, DEFAULT_WORKLOAD_IDENTITY};
use crate::host::snapshot::tests::RestoreFixture;
use crate::host::snapshot::{DESCRIPTION_FILE, KEY_FILE, OPENVMM_DIR};
use crate::protocol::root::RootImage;
use crate::protocol::v2::{self, Frame, Outcome};
use crate::protocol::{
    OUTER_DATA, OUTER_HEADER_LEN, OUTER_HOST_ATTACH, OUTER_READY, decode_outer_header,
    encode_exec_payload, encode_maps_payloads, encode_outer, status,
};

/// The codes of the requests of NVXC that the fake guest of the fixture's restores serves: the
/// activate message, the mapping table and the workload's Start, and the end of its standard
/// input.
const ACTIVATE: u8 = 0x08;
/// See [`ACTIVATE`].
const MAPS: u8 = 0x06;
/// See [`ACTIVATE`].
const EXEC: u8 = 0x02;
/// See [`ACTIVATE`].
const STDIN_END: u8 = 0x0a;

/// The fake VMM and guest that the fixture's restores spawn in place of OpenVMM.
const HELPER: &str = "host::cli::sandbox::lifecycle::start::tests::restore::restore_helper";
/// OpenVMM's restore-ready event.
const READY_EVENT: &[u8] = b"OPENVMM_RESTORE_READY_V1\n";
/// The workload of the fixture's restored sandboxes.
const WORKLOAD: [&str; 2] = ["/bin/echo", "restored"];

/// The name of the fixture's snapshot.
fn name() -> Name {
    Name::try_from("source".to_owned()).unwrap()
}

/// Returns the activation record of an activate request's `payload`, after the workload's UID,
/// GID, and account flag.
fn activation(payload: &[u8]) -> (GuestRestoreRecord, (u32, u32, u8)) {
    let id = |at: usize| u32::from_le_bytes(payload[at..at + 4].try_into().unwrap());
    let record = GuestRestoreRecord::decode(&payload[9..]).unwrap();
    (record, (id(0), id(4), payload[8]))
}

/// A store whose one snapshot, `source`, restores the configuration of a [`RestoreFixture`],
/// and the fake VMs that the fixture's restores spawned, which it ends.
struct Fixture {
    /// The store, the state directories, and the fake VMs' recipes and traces.
    root: tempfile::TempDir,
    /// The configuration and runtime that the snapshot restores.
    restore: RestoreFixture,
    /// The store.
    store: Store,
    /// Each fake VM, with the OpenVMM arguments that it replaced, and whether every VM spawned
    /// before it had exited when it spawned.
    spawned: Mutex<Vec<(ProcessRef, Vec<OsString>, bool)>>,
}

impl Fixture {
    fn new(nic: bool) -> Self {
        let restore = RestoreFixture::current(nic);
        let root = tempfile::tempdir().unwrap();
        let store = Store::create(&root.path().join("store")).unwrap();
        let snapshot = restore.opened();
        {
            let held = store.lock(&name(), Deadline::after(10.0)).unwrap();
            let staged = held.stage().unwrap();
            for entry in [KEY_FILE, DESCRIPTION_FILE, OPENVMM_DIR] {
                fs::rename(snapshot.dir.join(entry), staged.dir().join(entry)).unwrap();
            }
            held.publish(staged).unwrap();
        }
        Self {
            root,
            restore,
            store,
            spawned: Mutex::new(Vec::new()),
        }
    }

    fn state(&self, state: &str) -> PathBuf {
        self.root.path().join(state)
    }

    fn source(&self) -> Source {
        Source {
            snapshot: name(),
            store: self.root.path().join("store"),
            workload: WORKLOAD.map(str::to_owned).to_vec(),
        }
    }

    /// Restores the snapshot into the state directory `state`, whose fake VM does as `behavior`
    /// says, as a start of a sandbox restored from it does.
    fn start(&self, state: &str, config: VmConfig, behavior: &str, timeout: f64) -> Result {
        self.start_with(state, config, behavior, timeout, false)
    }

    /// Restores the snapshot as [`start`](Self::start) does, forwarding the status of a workload
    /// that does not exit with status 0 if `forward_status`, as `run --restore-snapshot` asks.
    fn start_with(
        &self,
        state: &str,
        config: VmConfig,
        behavior: &str,
        timeout: f64,
        forward_status: bool,
    ) -> Result {
        let state_dir = self.state(state);
        let mut lock = LifecycleLock::acquire(&state_dir, timeout, true).unwrap();
        let source = self.source();
        let restore = Restore {
            source: &source,
            identity: (1000, 1001),
            forward_status,
        };
        launch_locked(
            &mut lock,
            &self.restore.runtime,
            config,
            Some(restore),
            None,
            timeout,
            |prepared| self.spawn(&state_dir, prepared, behavior),
        )
    }

    /// Spawns the fake VM of `behavior` in place of the OpenVMM of `prepared`.
    fn spawn(
        &self,
        state_dir: &Path,
        prepared: Prepared,
        behavior: &str,
    ) -> crate::Result<Launched> {
        let arguments = prepared.arguments().to_vec();
        let ready = arguments
            .windows(2)
            .find(|pair| pair[0] == "--restore-ready-path")
            .map(|pair| pair[1].to_str().unwrap().to_owned())
            .unwrap();
        let recipe_path = state_dir.with_extension("json");
        let recipe = json!({
            "endpoint": prepared.outputs().endpoint, "ready": ready, "behavior": behavior,
            "trace": state_dir.with_extension("trace"), "outcome": prepared.outputs().outcome,
        });
        fs::write(&recipe_path, serde_json::to_vec(&recipe).unwrap()).unwrap();
        let mut spawned = self.spawned.lock().unwrap();
        let exited = spawned
            .iter()
            .all(|(reference, ..)| matches!(Process::open(reference), Ok(Opened::Exited(_))));
        // The helper starts where a VMM starts, which is not its VM directory on Windows, so its
        // recipe's path follows libtest's own arguments, as a filter that names no test.
        let mut helper = ["--exact", HELPER, "--ignored", "--nocapture"]
            .map(OsString::from)
            .to_vec();
        helper.push(recipe_path.into_os_string());
        let launched = vm::spawn(prepared.run_instead(std::env::current_exe().unwrap(), helper))?;
        spawned.push((launched.process.reference().clone(), arguments, exited));
        Ok(launched)
    }

    /// Returns the kind and payload of each NVXC request of the fake VM of `state`, in order.
    fn requests(&self, state: &str) -> Vec<(u8, Vec<u8>)> {
        fs::read_to_string(self.state(state).with_extension("trace"))
            .unwrap_or_default()
            .lines()
            .map(|line| {
                let request: Value = serde_json::from_str(line).unwrap();
                let payload = request["payload"].as_str().unwrap();
                let payload = (0..payload.len())
                    .step_by(2)
                    .map(|at| u8::from_str_radix(&payload[at..at + 2], 16).unwrap())
                    .collect();
                (request["kind"].as_u64().unwrap() as u8, payload)
            })
            .collect()
    }

    fn spawned(&self) -> Vec<(ProcessRef, Vec<OsString>, bool)> {
        self.spawned.lock().unwrap().clone()
    }

    /// Returns whether the fake VM `index` has exited, which it waits a few seconds for.
    fn exited(&self, index: usize) -> bool {
        match Process::open(&self.spawned()[index].0).unwrap() {
            Opened::Exited(_) => true,
            Opened::Running(process) => process
                .wait(Instant::now() + Duration::from_secs(5))
                .unwrap()
                .is_some(),
        }
    }

    /// Returns whether another restore could take the snapshot's lock at once.
    fn snapshot_free(&self) -> bool {
        self.store.lock(&name(), Deadline::after(0.0)).is_ok()
    }

    /// Returns how many entries the view of the snapshot in the state directory `state` has.
    fn view(&self, state: &str) -> usize {
        fs::read_dir(self.state(state).join(OPENVMM_DIR)).map_or(0, |entries| entries.count())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for (reference, ..) in self.spawned() {
            if let Ok(Opened::Running(process)) = Process::open(&reference) {
                let _ = process.kill();
                let _ = process.wait(Instant::now() + KILL_TIMEOUT);
            }
        }
    }
}

/// Returns the configuration of a restore of `fixture`, of a GPT image if `gpt`, with an upper in
/// RAM if `ram`, one read-only share, and a policy that allows egress and publishes the guest's
/// listeners.
fn restored_config(fixture: &Fixture, gpt: bool, ram: bool) -> VmConfig {
    let mut config = fixture.restore.config.clone();
    let RootConfig::Layered(root) = &mut config.root else {
        unreachable!("the fixture restores a layered root");
    };
    if gpt {
        root.format = RootImage::gpt(None);
        root.gpt_first_partition_bound = Some(512 * 1024);
    }
    if ram {
        root.upper = UpperConfig::Ram {
            max_memory_percent: 33,
        };
    }
    let shared = fixture.root.path().join("work");
    fs::create_dir_all(&shared).unwrap();
    config.filesystem = Some(ExportPlan {
        children: vec![ExportChild {
            root: shared,
            file: false,
            writable: false,
            denied: Vec::new(),
            allowed: Vec::new(),
            write: Vec::new(),
        }],
        binds: vec![Bind {
            child: 0,
            source: String::new(),
            target: "/work".to_owned(),
            read_only: true,
        }],
        owner: Owner::Vmm,
    });
    let mut policy = NetworkPolicy::new(Access::Allow);
    policy.ingress = Access::Allow;
    config.network.as_mut().unwrap().policy = policy;
    config
}

#[test]
fn named_snapshots_restore_each_image_format_and_upper_with_their_own_policy_and_workload() {
    for (gpt, ram) in [(false, false), (false, true), (true, false), (true, true)] {
        let fixture = Fixture::new(true);
        let config = restored_config(&fixture, gpt, ram);
        let maps = config.mapping_table();
        fixture.start("state", config, "ready", 30.0).unwrap();
        let requests = fixture.requests("state");
        let kinds: Vec<u8> = requests.iter().map(|(kind, _)| *kind).collect();
        assert_eq!(kinds, [ACTIVATE, MAPS, EXEC, STDIN_END]);
        let (record, identity) = activation(&requests[0].1);
        assert_eq!(identity, (1000, 1001, 0));
        let configuration = record.configuration();
        // The restore's own policy gives the guest its DNS server and announces its listeners.
        assert_eq!(
            configuration.network,
            Some(Network {
                dns: Some("10.42.0.1".parse().unwrap()),
                publication: Publication::Published,
                proxy: None,
            })
        );
        assert_eq!(matches!(configuration.image, Image::Gpt { .. }), gpt);
        assert_eq!(matches!(configuration.upper, Upper::Tmpfs { .. }), ram);
        assert_eq!(configuration.mappings, 1);
        // The snapshot's two online processors, of four.
        assert_eq!(configuration.resources.cpus, 2);
        assert_eq!(
            requests[1].1,
            encode_maps_payloads(&maps, v2::MAX_PAYLOAD).unwrap()[0]
        );
        let workload = protocol::Workload {
            argv: WORKLOAD.map(str::to_owned).to_vec(),
            timeout_ms: 0,
            cwd: None,
            environment: WorkloadEnvironment::Default,
        };
        assert_eq!(requests[2].1, encode_exec_payload(&workload).unwrap());
        let [(reference, arguments, _)] = &fixture.spawned()[..] else {
            panic!("one VM restores");
        };
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair[0] == "--network-ingress" && pair[1] == "allow")
        );
        assert!(
            arguments
                .iter()
                .any(|argument| argument == "--restore-network-policy")
        );
        assert_eq!(
            arguments
                .iter()
                .any(|argument| argument == "--restore-absent-scratch"),
            ram
        );
        // The VM runs on, committed with its view of the snapshot, whose lock is free again.
        let locked = StateDir::open(&fixture.state("state"))
            .unwrap()
            .lock(Deadline::after(5.0))
            .unwrap();
        assert_eq!(&locked.runtime().unwrap().unwrap().process, reference);
        assert!(locked.journal().unwrap().is_none());
        assert!(!fixture.exited(0));
        assert_eq!(fixture.view("state"), 3);
        assert!(fixture.snapshot_free());
    }
}

/// Returns `run ARGUMENTS`, parsed.
fn run_options(arguments: &[&str]) -> run::Options {
    let arguments: Vec<String> = arguments
        .iter()
        .map(|&argument| argument.to_owned())
        .collect();
    run::parse(&arguments).unwrap().0
}

#[test]
fn a_one_off_restore_installs_the_network_options_of_run_as_its_own_policy() {
    let fixture = Fixture::new(true);
    let options = run_options(&[
        "--restore-snapshot",
        "source",
        "--network-profile",
        "portable",
        "--network-egress",
        "deny",
        "--network-egress-allow",
        "10.42.0.1/32:udp:53",
        "--host-loopback",
        "allow",
        "--host-loopback-forward",
        "tcp:18080:8080",
    ]);
    // The run's options at the NIC of the snapshot, as `sandbox` lowers them and a start of the
    // sandbox records them.
    let nic = fixture
        .store
        .lock(&name(), Deadline::after(5.0))
        .unwrap()
        .open()
        .unwrap()
        .nic();
    let network = run::restored_network(options.network, nic).unwrap();
    let rules = network.egress_rules().unwrap();
    let mut config = fixture.restore.config.clone();
    config.network = sandbox_network(&network, &rules).unwrap();
    fixture.start("state", config, "ready", 30.0).unwrap();
    // The guest learns the DNS server that the policy lets it reach, and announces its listeners,
    // which the policy publishes on host loopback.
    let requests = fixture.requests("state");
    assert_eq!(requests[0].0, ACTIVATE);
    assert_eq!(
        activation(&requests[0].1).0.configuration().network,
        Some(Network {
            dns: Some("10.42.0.1".parse().unwrap()),
            publication: Publication::Published,
            proxy: None,
        })
    );
    // OpenVMM installs the policy, egress, forwards, and host loopback, in place of the
    // snapshot's, at the NIC's address, which the restore does not name.
    let [(_, arguments, _)] = &fixture.spawned()[..] else {
        panic!("one VM restores");
    };
    for [option, value] in [
        ["--network-egress", "deny"],
        ["--network-egress-allow", "10.42.0.1/32:udp:53"],
        ["--host-loopback", "allow"],
        ["--host-loopback-forward", "tcp:18080:8080"],
    ] {
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair[0] == option && pair[1] == value),
            "{option} {value}: {arguments:?}"
        );
    }
    assert!(
        arguments
            .iter()
            .any(|argument| argument == "--restore-network-policy")
    );
    assert!(!arguments.iter().any(|argument| argument == "--net"));
}

#[test]
fn a_one_off_restore_refuses_the_network_options_of_a_snapshot_without_a_nic_before_it_provisions()
{
    let fixture = Fixture::new(false);
    let store = fixture.root.path().join("store");
    let state = fixture.state("state");
    let options = run_options(&[
        "--restore-snapshot",
        "source",
        "--store-dir",
        store.to_str().unwrap(),
        "--state-dir",
        state.to_str().unwrap(),
        "--layer",
        "distro,distro.erofs,11111111-1111-1111-1111-111111111111",
        "--upper",
        "ram",
        "--network-profile",
        "portable",
    ]);
    let error = run::run(None, options).unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::Unsupported), "{error:?}");
    assert!(error.message().unwrap().contains("no NIC"), "{error:?}");
    assert!(!state.exists());
    assert!(fixture.spawned().is_empty());
}

#[test]
fn a_one_off_restore_ends_with_the_status_of_its_workload_once_its_vm_and_launch_have_ended() {
    let fixture = Fixture::new(false);
    let config = &fixture.restore.config;
    // A forwarded status changes nothing for a workload that exits with status 0.
    fixture
        .start_with("ready", config.clone(), "ready", 30.0, true)
        .unwrap();
    // Another status ends the start with that status, in place of the error of `snapshot
    // restore`, once the VM and its launch have ended, before the snapshot goes free, and so does
    // a signal's, 128 plus its number, as `sandbox run` ends.
    for (index, (state, status)) in [("failing", 3), ("signaled", 143)].into_iter().enumerate() {
        let error = fixture
            .start_with(state, config.clone(), state, 30.0, true)
            .unwrap_err();
        assert!(
            matches!(error, Exit::Status(found) if found == status),
            "{error:?}"
        );
        assert!(fixture.exited(index + 1));
        let locked = StateDir::open(&fixture.state(state))
            .unwrap()
            .lock(Deadline::after(5.0))
            .unwrap();
        assert!(locked.journal().unwrap().is_none());
        assert!(locked.runtime().unwrap().is_none());
        assert!(fixture.snapshot_free());
    }
}

#[test]
fn concurrent_restores_of_one_snapshot_wait_for_its_lock_until_the_first_vm_has_exited() {
    let fixture = Fixture::new(false);
    let config = &fixture.restore.config;
    let (first, second) = thread::scope(|scope| {
        let first = scope.spawn(|| fixture.start("first", config.clone(), "stall", 2.0));
        let until = Instant::now() + Duration::from_secs(20);
        while fixture.spawned().is_empty() {
            assert!(Instant::now() < until, "the first restore spawned no VM");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!fixture.snapshot_free());
        let second = fixture.start("second", config.clone(), "ready", 30.0);
        (first.join().unwrap(), second)
    });
    // The first VM never became ready, so its restore timed out and ended it, and the second VM
    // started only once the first had exited.
    assert_eq!(first.unwrap_err().code(), Some(ErrorCode::TimedOut));
    second.unwrap();
    let spawned = fixture.spawned();
    assert_eq!(spawned.len(), 2);
    assert!(spawned[1].2);
    assert!(fixture.exited(0));
    assert!(fixture.requests("first").is_empty());
    assert_eq!(fixture.requests("second").len(), 3);
    assert!(fixture.snapshot_free());
}

#[test]
fn a_failed_restore_ends_its_vm_and_its_launch_before_its_snapshot_goes_free() {
    for (behavior, code, message) in [
        ("malformed", ErrorCode::Protocol, "snapshot restore failed"),
        ("exit", ErrorCode::Exited, "OpenVMM exited during startup"),
        (
            "failing",
            ErrorCode::Other,
            "the restored workload ended with Exit status 3",
        ),
        // A guest that hangs up proves no exit: as for a cold start, its VM has the grace to exit
        // by itself before the start ends it.
        ("hangup", ErrorCode::Exited, "OpenVMM exited during startup"),
    ] {
        let fixture = Fixture::new(false);
        let error = fixture
            .start("state", fixture.restore.config.clone(), behavior, 30.0)
            .unwrap_err();
        assert_eq!(error.code(), Some(code), "{behavior}: {error:?}");
        assert!(
            error.message().is_some_and(|text| text.contains(message)),
            "{behavior}: {error:?}"
        );
        assert!(fixture.exited(0), "{behavior}");
        let locked = StateDir::open(&fixture.state("state"))
            .unwrap()
            .lock(Deadline::after(5.0))
            .unwrap();
        assert!(locked.journal().unwrap().is_none(), "{behavior}");
        assert!(locked.runtime().unwrap().is_none(), "{behavior}");
        assert!(fixture.snapshot_free(), "{behavior}");
    }
}

/// A restore whose guest fails before it is ready reports what the restore's evidence attributes
/// the failure to: the request, for a guest that ends its VM with the status of a root that does
/// not assemble, after the VM's outcome, the host, for an outcome report that cannot be read,
/// after the report's error, and nothing that it establishes for a malformed Ready.
#[test]
fn a_restore_that_fails_before_its_guest_is_ready_reports_its_attribution() {
    for (behavior, code, prefix, attribution) in [
        (
            "unusable-root",
            ErrorCode::Exited,
            "OpenVMM exited during startup: guest-exit status 54; restore failed during ",
            ": UnusableRoot, attributed to the request",
        ),
        (
            "unreadable-report",
            ErrorCode::Io,
            "cannot parse outcome report ",
            ": ReportUnreadable, attributed to the host",
        ),
        (
            "malformed",
            ErrorCode::Protocol,
            "snapshot restore failed: restore failed during ",
            ": Malformed, of unknown attribution",
        ),
    ] {
        let fixture = Fixture::new(false);
        let error = fixture
            .start("state", fixture.restore.config.clone(), behavior, 30.0)
            .unwrap_err();
        assert_eq!(error.code(), Some(code), "{behavior}: {error:?}");
        let message = error.message().unwrap_or_default();
        assert!(
            message.starts_with(prefix) && message.ends_with(attribution),
            "{behavior}: {message}"
        );
        assert!(fixture.exited(0), "{behavior}");
        assert!(fixture.snapshot_free(), "{behavior}");
    }
}

#[test]
fn a_restore_whose_spawn_cannot_confirm_that_its_vm_ended_recovers_it_before_its_snapshot_goes_free()
 {
    let fixture = Fixture::new(false);
    let state_dir = fixture.state("state");
    let mut lock = LifecycleLock::acquire(&state_dir, 30.0, true).unwrap();
    let source = fixture.source();
    let restore = Restore {
        source: &source,
        identity: (1000, 1001),
        forward_status: false,
    };
    let error = launch_locked(
        &mut lock,
        &fixture.restore.runtime,
        fixture.restore.config.clone(),
        Some(restore),
        None,
        30.0,
        |prepared| {
            // The VM runs, but the spawn cannot confirm that it ended.
            fixture.spawn(&state_dir, prepared, "stall")?;
            Err(crate::Error::new(
                ErrorKind::StillRunning,
                "the spawn cannot confirm that its VM ended",
            ))
        },
    )
    .unwrap_err();
    assert!(
        error
            .message()
            .is_some_and(|text| text.contains("cannot confirm that its VM ended")),
        "{error:?}"
    );
    // The journal's recovery ended the VM, which nothing recorded, before the snapshot went free.
    assert!(fixture.exited(0));
    assert!(lock.locked().journal().unwrap().is_none());
    assert!(fixture.snapshot_free());
}

#[test]
fn a_later_start_restores_again_with_an_upper_in_ram_but_never_with_a_scratch_image_it_gave() {
    for ram in [true, false] {
        let fixture = Fixture::new(false);
        let mut config = fixture.restore.config.clone();
        if ram {
            let RootConfig::Layered(root) = &mut config.root else {
                unreachable!("the fixture restores a layered root");
            };
            root.upper = UpperConfig::Ram {
                max_memory_percent: 33,
            };
        }
        fixture
            .start("state", config.clone(), "ready", 30.0)
            .unwrap();
        let Opened::Running(first) = Process::open(&fixture.spawned()[0].0).unwrap() else {
            panic!("the restored VM runs");
        };
        first.kill().unwrap();
        assert!(fixture.exited(0));
        LifecycleLock::acquire(&fixture.state("state"), 5.0, false)
            .unwrap()
            .clear_runtime()
            .unwrap();
        let again = fixture.start("state", config, "ready", 30.0);
        if ram {
            again.unwrap();
            assert_eq!(fixture.spawned().len(), 2);
            assert_eq!(fixture.requests("state").len(), 6);
        } else {
            // The guest of the first start may have written the image, which a restore's guest
            // refuses: nothing launches, and the view stays for deprovision.
            let error = again.unwrap_err();
            assert_eq!(error.code(), Some(ErrorCode::Unsupported), "{error:?}");
            assert_eq!(fixture.spawned().len(), 1);
            assert_eq!(fixture.requests("state").len(), 3);
        }
        assert_eq!(fixture.view("state"), 3, "{ram}");
        assert!(fixture.snapshot_free());
    }
}

#[test]
fn a_restore_whose_command_dies_before_ready_never_activates_and_recovery_ends_its_vm() {
    let fixture = Fixture::new(false);
    let state_dir = fixture.state("state");
    vm::create_private_dir(&state_dir).unwrap();
    let state = StateDir::open(&state_dir).unwrap();
    {
        // What a restore's start does before its guest is ready.
        let held = fixture.store.lock(&name(), Deadline::after(5.0)).unwrap();
        let snapshot = held.open().unwrap();
        let runtime = &fixture.restore.runtime;
        let vm = VmDir::open(&state_dir).unwrap();
        let prepared = vm::create(runtime, &fixture.restore.config, Some(&snapshot), &vm).unwrap();
        let mut locked = state.lock(Deadline::after(5.0)).unwrap();
        let mut launch = locked.begin_launch(&prepared, &runtime.identity).unwrap();
        let launched = fixture.spawn(&state_dir, prepared, "stall").unwrap();
        launch.record_process(launched.process.reference()).unwrap();
        // The command dies here: its locks go, and its journal, VM, and view stay.
    }
    assert!(fixture.snapshot_free());
    assert!(!fixture.exited(0));
    let recovered = state.lock(Deadline::after(10.0)).unwrap();
    assert!(fixture.exited(0));
    assert!(recovered.journal().unwrap().is_none());
    assert!(recovered.runtime().unwrap().is_none());
    assert!(fixture.requests("state").is_empty());
    assert_eq!(fixture.view("state"), 3);
}

#[test]
fn deprovision_removes_a_restored_sandboxs_view_only_under_its_snapshots_lock() {
    let fixture = Fixture::new(false);
    let state_dir = fixture.state("state");
    vm::create_private_dir(&state_dir).unwrap();
    {
        let held = fixture.store.lock(&name(), Deadline::after(5.0)).unwrap();
        let snapshot = held.open().unwrap();
        let vm = VmDir::open(&state_dir).unwrap();
        let runtime = &fixture.restore.runtime;
        drop(vm::create(runtime, &fixture.restore.config, Some(&snapshot), &vm).unwrap());
    }
    let source = fixture.source();
    // The record of a restored sandbox, of an EROFS image with an upper in RAM.
    let image = fixture.root.path().join("distro.erofs");
    fs::write(&image, b"distro layer\n").unwrap();
    let layer = SandboxLayer::new(
        LayerRole::Distro,
        image.to_str().unwrap(),
        "11111111-1111-1111-1111-111111111111",
    );
    let binding = DistroBinding::verify(&layer).unwrap();
    let options = ProvisionConfig {
        hypervisor: "kvm",
        memory_mib: 512,
        cmdline: "",
        ram_upper_percent: None,
        hostname: DEFAULT_HOSTNAME,
        workload_identity: DEFAULT_WORKLOAD_IDENTITY,
        export: None,
        network: None,
    };
    let distro = RecordedDistro::new(&layer, binding).unwrap();
    let text = serialize_launch(&distro, None, &options, Some(&source)).unwrap();
    write_json(&state_dir.join(CONFIG_NAME), &text).unwrap();
    let state_path = state_dir.to_str().unwrap();
    let artifacts = Artifacts { root: None };
    let held = fixture.store.lock(&name(), Deadline::after(5.0)).unwrap();
    let error = deprovision(state_path, &artifacts, 0.5).unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::SandboxBusy), "{error:?}");
    assert_eq!(fixture.view("state"), 3);
    assert!(state_dir.join(CONFIG_NAME).exists());
    drop(held);
    deprovision(state_path, &artifacts, 5.0).unwrap();
    assert!(!state_dir.exists());
    // The snapshot's own files stay whole.
    fixture
        .store
        .lock(&name(), Deadline::after(5.0))
        .unwrap()
        .open()
        .unwrap();
}

/// A start of a restored sandbox refuses a share of its snapshot store, through which the guest
/// could change the snapshot's files that the view of each restore links, before it takes the
/// snapshot's lock or launches anything.
#[test]
fn a_restored_start_refuses_a_share_of_its_snapshot_store() {
    let fixture = Fixture::new(false);
    let state_dir = fixture.state("state");
    vm::create_private_dir(&state_dir).unwrap();
    let image = fixture.root.path().join("distro.erofs");
    fs::write(&image, b"distro layer\n").unwrap();
    let layer = SandboxLayer::new(
        LayerRole::Distro,
        image.to_str().unwrap(),
        "11111111-1111-1111-1111-111111111111",
    );
    let binding = DistroBinding::verify(&layer).unwrap();
    let store = fixture.root.path().join("store");
    let mounts = sandbox_mounts(&[format!("/data,{},rw", store.display())], &[], None).unwrap();
    validate_mounts(&mounts).unwrap();
    let export = plan_mounts(&mounts).unwrap();
    let options = ProvisionConfig {
        hypervisor: "kvm",
        memory_mib: 512,
        cmdline: "",
        ram_upper_percent: None,
        hostname: DEFAULT_HOSTNAME,
        workload_identity: DEFAULT_WORKLOAD_IDENTITY,
        export: export.as_ref(),
        network: None,
    };
    let distro = RecordedDistro::new(&layer, binding).unwrap();
    let source = fixture.source();
    let text = serialize_launch(&distro, None, &options, Some(&source)).unwrap();
    write_json(&state_dir.join(CONFIG_NAME), &text).unwrap();
    let root = fixture.state("checkout");
    let artifacts = Artifacts {
        root: Some(root.to_str().unwrap()),
    };
    let error = start(state_dir.to_str().unwrap(), &artifacts, 5.0, false).unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::Unsupported), "{error:?}");
    let message = error.message().unwrap_or_default();
    assert!(
        message.contains("overlaps the sandbox's snapshot store"),
        "{message}"
    );
    assert_eq!(fixture.view("state"), 0);
    assert!(fixture.spawned().is_empty());
}

/// Returns an artifact root with the fakes and provenance whose presence a start checks before
/// it launches anything, which no start of these tests runs.
fn artifact_root(root: &Path) -> String {
    let dir = ArtifactDir::from_dir(root).unwrap();
    for (artifact, provenance, field) in [
        (dir.openvmm(), OPENVMM_PROVENANCE_NAME, "executable_sha256"),
        (dir.kernel(), KERNEL_PROVENANCE_NAME, "kernel_sha256"),
        (
            dir.initramfs(Guest::Alpine),
            INITRAMFS_PROVENANCE_NAME,
            "initramfs_sha256",
        ),
    ] {
        for (path, contents) in [
            (artifact, "fake".to_owned()),
            (
                dir.artifact(provenance),
                json!({ "format": 1, field: "ab".repeat(32) }).to_string(),
            ),
        ] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
    }
    root.to_str().unwrap().to_owned()
}

/// `snapshot restore` provisions a sandbox whose configuration records the snapshot, the store as
/// the command resolved it, and the workload, an exec request that may hold whitespace and
/// quotes, which no boot token can, and starts it as a restore of that snapshot: a start with
/// memory that the snapshot refuses, which only a restore checks, fails before it launches or
/// links anything, and leaves the sandbox provisioned.
#[test]
fn snapshot_restore_records_its_source_and_starts_a_restore_of_it() {
    let fixture = Fixture::new(false);
    let root = artifact_root(&fixture.state("checkout"));
    let layer = fixture.state("distro.erofs");
    fs::write(&layer, "distro").unwrap();
    let store = fixture.root.path().join("store");
    let state = fixture.state("command");
    let workload = ["/bin/echo", "a \"restored\" sandbox"];
    let arguments = [
        "restore",
        "source",
        "--store-dir",
        store.to_str().unwrap(),
        "--state-dir",
        state.to_str().unwrap(),
        "--layer",
        &format!(
            "distro,{},11111111-1111-1111-1111-111111111111",
            layer.display()
        ),
        "--upper",
        "ram",
        "--hypervisor",
        if cfg!(windows) { "whp" } else { "kvm" },
        "--memory-mib",
        "2176",
        "--entrypoint",
        workload[0],
        "--arg",
        workload[1],
        "--timeout",
        "10",
    ]
    .map(str::to_owned);
    let (command, unrecognized) = snapshots::parse(&arguments).unwrap();
    assert!(unrecognized.is_empty(), "{unrecognized:?}");
    let error = snapshots::run(Some(&root), command).unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::Unsupported), "{error:?}");
    let message = error.message().unwrap_or_default();
    assert!(
        message.contains("snapshot memory must be from boot"),
        "{message}"
    );
    let record = read_config(&state.join(CONFIG_NAME)).unwrap();
    assert_eq!(record["source_snapshot"], "source");
    assert_eq!(record["workload"], json!(workload));
    assert_eq!(
        Config::new(record).unwrap().source().unwrap(),
        Some(Source {
            snapshot: name(),
            store: resolve(&store).unwrap(),
            workload: workload.map(str::to_owned).to_vec(),
        })
    );
    assert_eq!(fixture.view("command"), 0);
    assert!(fixture.snapshot_free());
    let artifacts = Artifacts {
        root: Some(root.as_str()),
    };
    deprovision(state.to_str().unwrap(), &artifacts, 5.0).unwrap();
    assert!(!state.exists());
}

/// `run --restore-snapshot` takes the workloads that `snapshot restore` takes, exec requests that
/// may hold whitespace and quotes, which no boot token can: its one-off provisions the sandbox and
/// starts a restore of the snapshot, which memory that the snapshot refuses ends before it
/// launches or links anything, and then removes the sandbox.
#[test]
fn a_one_off_restore_takes_the_workloads_of_snapshot_restore() {
    let fixture = Fixture::new(false);
    let root = artifact_root(&fixture.state("checkout"));
    let layer = fixture.state("distro.erofs");
    fs::write(&layer, "distro").unwrap();
    let layer = format!(
        "distro,{},11111111-1111-1111-1111-111111111111",
        layer.display()
    );
    let store = fixture.root.path().join("store");
    let state = fixture.state("one-off");
    let options = run_options(&[
        "--restore-snapshot",
        "source",
        "--store-dir",
        store.to_str().unwrap(),
        "--state-dir",
        state.to_str().unwrap(),
        "--layer",
        &layer,
        "--upper",
        "ram",
        "--hypervisor",
        if cfg!(windows) { "whp" } else { "kvm" },
        "--memory-mib",
        "2176",
        "--entrypoint",
        "/bin/echo",
        "--arg",
        "a \"restored\" sandbox",
        "--timeout",
        "10",
    ]);
    let error = run::run(Some(&root), options).unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::Unsupported), "{error:?}");
    let message = error.message().unwrap_or_default();
    assert!(
        message.contains("snapshot memory must be from boot"),
        "{message}"
    );
    assert!(!state.exists());
    assert!(fixture.snapshot_free());
}

/// Reads one record of the control endpoint, or `None` once the host has gone.
fn read_record(stream: &mut impl Read) -> Option<(u8, Vec<u8>)> {
    let mut header = [0; OUTER_HEADER_LEN];
    stream.read_exact(&mut header).ok()?;
    let header = decode_outer_header(&header).unwrap();
    let mut payload = vec![0; header.length];
    stream.read_exact(&mut payload).ok()?;
    Some((header.record_type, payload))
}

/// Serves one session of the host as the guest of the recipe's behavior does, and traces its
/// requests, and the end of the standard input of the workload that the Start runs.
fn serve(stream: &mut (impl Read + Write), capability: &[u8], epoch: u64, recipe: &Value) {
    let Some((kind, payload)) = read_record(stream) else {
        return;
    };
    assert_eq!((kind, payload.as_slice()), (OUTER_HOST_ATTACH, capability));
    let ready = encode_outer(OUTER_READY, &[7; 16], epoch, 0, &[]).unwrap();
    if stream.write_all(&ready).is_err() {
        return;
    }
    let trace = recipe["trace"].as_str().unwrap();
    // Traces the frame of a DATA record, and returns its kind, its request ID and its payload.
    let request = |frame: &[u8]| {
        let (request_id, _) = Frame::decode(frame).unwrap();
        let (kind, payload) = (frame[5], &frame[24..]);
        let line = json!({"kind": kind, "payload": hex(payload)});
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(trace)
            .unwrap();
        writeln!(file, "{line}").unwrap();
        (kind, request_id, payload.to_vec())
    };
    let mut sequence = 1;
    while let Some((kind, frame)) = read_record(stream) {
        assert_eq!(kind, OUTER_DATA);
        let (kind, request_id, payload) = request(&frame);
        // A guest that hangs up does once it has the workload's Start, which it never answers.
        if kind == EXEC && recipe["behavior"] == "hangup" {
            return;
        }
        let ready = |activation: &[u8]| {
            let frame = Frame::Ready {
                version: v2::VERSION,
                activation,
            };
            frame.encode(request_id).unwrap()
        };
        let replies = match kind {
            ACTIVATE if recipe["behavior"] == "malformed" => vec![ready(&[0])],
            // The guest cannot assemble its root, and powers off with the status of the failure's
            // class, which OpenVMM reports, and then exits with, or reports unreadably.
            ACTIVATE
                if matches!(
                    recipe["behavior"].as_str(),
                    Some("unusable-root" | "unreadable-report")
                ) =>
            {
                let code = status::GUEST_RESTORE_ROOT;
                let report = json!({
                    "schema_version": 1,
                    "outcome": {"operation": "managed", "category": "guest-exit", "status_code": code},
                    "teardown": {"workers": true},
                });
                let report = match recipe["behavior"].as_str() {
                    Some("unreadable-report") => "{ truncated".to_owned(),
                    _ => report.to_string(),
                };
                fs::write(recipe["outcome"].as_str().unwrap(), report).unwrap();
                std::process::exit(code.into());
            }
            ACTIVATE => vec![ready(
                &GuestRestoreReady::of(&activation(&payload).0).encode(),
            )],
            MAPS => vec![ready(&[])],
            // The workload reads its standard input until its end, which follows the Start,
            // writes its output, then exits, with status 3 if it fails, or SIGTERM ends it.
            _ => {
                let (_, end) = read_record(stream).unwrap();
                assert_eq!(request(&end), (STDIN_END, request_id, Vec::new()));
                let (status, outcome) = match recipe["behavior"].as_str() {
                    Some("failing") => (3, Outcome::Exit),
                    Some("signaled") => (143, Outcome::Signal),
                    _ => (0, Outcome::Exit),
                };
                [
                    Frame::Stdout(b"restored\n"),
                    Frame::Exit { status, outcome },
                ]
                .map(|frame| frame.encode(request_id).unwrap())
                .to_vec()
            }
        };
        for reply in replies {
            let record = encode_outer(OUTER_DATA, &[7; 16], epoch, sequence, &reply).unwrap();
            if stream.write_all(&record).is_err() {
                return;
            }
            sequence += 1;
        }
    }
}

#[test]
#[ignore = "the fake VMM and guest of a test restore; not real-VM restore proof"]
fn restore_helper() {
    let mut capability = Vec::new();
    std::io::stdin().read_to_end(&mut capability).unwrap();
    let recipe = PathBuf::from(std::env::args_os().last().unwrap());
    let recipe: Value = serde_json::from_slice(&fs::read(recipe).unwrap()).unwrap();
    let field = |name: &str| recipe[name].as_str().unwrap().to_owned();
    if recipe["behavior"] == "exit" {
        std::process::exit(3);
    }
    #[cfg(target_os = "linux")]
    let server = std::os::unix::net::UnixListener::bind(field("endpoint")).unwrap();
    #[cfg(windows)]
    let server = platform::tests::control_server(&field("endpoint"));
    if recipe["behavior"] == "stall" {
        // The VM serves its control endpoint, but its guest never becomes ready.
        loop {
            #[cfg(target_os = "linux")]
            thread::sleep(Duration::from_secs(1));
            #[cfg(windows)]
            {
                platform::tests::accept_control(&server);
                // Each client keeps the pipe until it hangs up.
                let _ = std::io::copy(&mut &server, &mut std::io::sink());
                platform::tests::disconnect_control(&server);
            }
        }
    }
    #[cfg(target_os = "linux")]
    let mut event = std::os::unix::net::UnixStream::connect(field("ready")).unwrap();
    #[cfg(windows)]
    let mut event = OpenOptions::new()
        .write(true)
        .open(field("ready").replace('/', "\\"))
        .unwrap();
    event.write_all(READY_EVENT).unwrap();
    drop(event);
    for epoch in 1.. {
        #[cfg(target_os = "linux")]
        let mut stream = server.accept().unwrap().0;
        #[cfg(windows)]
        let mut stream = {
            platform::tests::accept_control(&server);
            &server
        };
        serve(&mut stream, &capability, epoch, &recipe);
        #[cfg(windows)]
        platform::tests::disconnect_control(&server);
        if recipe["behavior"] == "hangup" {
            // The VM exits by itself, a while after its guest hung up.
            #[cfg(target_os = "linux")]
            drop(stream);
            thread::sleep(Duration::from_secs(2));
            std::process::exit(4);
        }
    }
}
