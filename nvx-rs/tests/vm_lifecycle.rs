//! A cold VM's lifecycle through the public API alone, on a real hypervisor.
//!
//! [`vm::create`] checks the launch and prepares it without starting it, [`vm::spawn`] starts it,
//! and [`Launched::connect`](vm::Launched::connect) opens its control connection. The caller then
//! saves the values of [`SavedVm`] and the VM's [`VmOutputs`], and lets the launch and the
//! connection go. [`vm::attach`] finds the VM again from those saved values alone, as a caller does
//! after it restarts, once for each of two connections, as each command of a CLI connects anew,
//! and [`vm::stop`] stops the VM through a request to its guest: the VM must exit on its own,
//! before any forced termination, and report its success and a complete teardown. The guest is
//! nvx-init, NVX's reference init, over the layered root that `nvx-rs sandbox start` boots: the
//! Ubuntu EROFS layer over an ext4 scratch upper.
//!
//! `scripts/nvx.py test-microvm --scenario vm-lifecycle` runs it on the selected backend, with
//! `NVX_VM_LIFECYCLE` set to the JSON object of its [`Inputs`], and then checks the outcome report
//! that OpenVMM wrote. The test keeps the VM's control capability in memory alone.
#![cfg(all(feature = "host", any(target_os = "linux", windows)))]

use std::env;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use nvx::host::config::{Boot, ConsoleAttachment, LayeredRoot, RootConfig, UpperConfig, VmConfig};
use nvx::host::process::{Opened, Process, ProcessRef};
use nvx::host::report::Report;
use nvx::host::runtime::{
    ArtifactIdentity, CompatibilityIdentity, Hypervisor, LaunchOptions, Runtime, RuntimeIdentity,
    probe_hypervisor,
};
use nvx::host::vm::{self, Connection, SavedVm, VmDir, VmOutputs};
use nvx::protocol::root::{Journal, OverlayMode, RootImage};
use nvx::protocol::view::{MachineIdMode, View};
use nvx::uuid::Uuid;

/// The caller's kernel command-line tokens, which `nvx-rs sandbox start` gives nvx-init too: it
/// manages the VM, and runs workloads as the fixed identity `65534:65534`.
const COMMAND_LINE: &str = "nvx_sandbox=1 nvx_mode=managed nvx_identity=65534:65534";

/// The request of NVXC, nvx-init's protocol, that asks the guest whether it is ready.
const PING: u8 = 1;
/// The request of NVXC that asks the guest to end the VM.
const STOP: u8 = 3;
/// NVXC's acknowledgement of [`PING`].
const READY: u8 = 0x81;
/// NVXC's acknowledgement of [`STOP`].
const STOPPED: u8 = 0x85;

/// The inputs of the test, which the scenario gives it as a JSON object in `NVX_VM_LIFECYCLE`.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Inputs {
    /// The selected backend: `kvm`, `mshv`, or `whp`.
    backend: String,
    /// The OpenVMM executable.
    openvmm: PathBuf,
    /// The SHA-256 digest of the OpenVMM executable, its identity in the runtime's.
    openvmm_sha256: String,
    /// The guest kernel.
    kernel: PathBuf,
    /// The SHA-256 digest of the guest kernel.
    kernel_sha256: String,
    /// The control initramfs, which holds nvx-init.
    initramfs: PathBuf,
    /// The SHA-256 digest of the control initramfs.
    initramfs_sha256: String,
    /// The Ubuntu EROFS layer.
    image: PathBuf,
    /// The UUID of the layer's filesystem.
    image_uuid: String,
    /// A new copy of the sandbox scratch template, the upper's image.
    scratch: PathBuf,
    /// The VM's directory, which the test creates.
    vm_dir: PathBuf,
    /// The longest wait for each step, in seconds.
    timeout_seconds: f64,
}

/// The VM that the test started, which it ends at once if the test fails before the VM exits.
///
/// It holds only what the caller saved, and finds the VM through it, as a caller that restarted
/// does.
struct Started {
    /// The VM's process.
    process: ProcessRef,
    /// The VM's output files and endpoints.
    outputs: VmOutputs,
    /// The longest wait for the VM's exit.
    timeout: Duration,
}

impl Drop for Started {
    fn drop(&mut self) {
        if let Ok(Opened::Running(process)) = Process::open(&self.process) {
            let now = Instant::now();
            if let Err(error) =
                vm::stop(&process, &self.outputs, |_| false, now, now + self.timeout)
            {
                eprintln!("the VM did not stop: {error}");
            }
        }
    }
}

/// The NVXC frame of `kind` for the request `request`, with status 0 and `payload`: the magic, the
/// version, 2, the kind, two zero flag bytes, the request's ID, the status, and the payload's
/// length, each integer little-endian, as version 2 of NVXC defines them.
fn frame(kind: u8, request: u64, payload: &[u8]) -> Vec<u8> {
    let length = u32::try_from(payload.len()).expect("a payload fits a frame");
    [
        &b"NVXC"[..],
        &[2, kind, 0, 0],
        &request.to_le_bytes(),
        &0i32.to_le_bytes(),
        &length.to_le_bytes(),
        payload,
    ]
    .concat()
}

/// Sends the request `kind` of the caller's own protocol with its guest, and checks that the guest
/// acknowledges it with `reply` by `deadline`; a `READY` names the protocol version, 2.
///
/// `nvx` carries the bytes of whatever protocol a caller speaks with its guest over the
/// [`Connection`] that `vm::attach` returns, and leaves the protocol to the caller. This caller's
/// guest is nvx-init, which speaks NVX's own protocol, NVXC; NVXC is private to `nvx`, so the
/// caller encodes the few frames that it sends and expects itself, as any caller of a guest of its
/// own would.
fn acknowledged(connection: &mut Connection, kind: u8, request: u64, reply: u8, deadline: Instant) {
    connection
        .send(&frame(kind, request, &[]), Some(deadline))
        .expect("the guest takes the request");
    let response = connection
        .receive(Some(deadline))
        .expect("the guest answers the request");
    let version: &[u8] = if reply == READY { &[2] } else { &[] };
    assert_eq!(
        response,
        frame(reply, request, version),
        "the guest acknowledges request {kind}"
    );
}

#[test]
#[ignore = "boots a VM; run scripts/nvx.py test-microvm --scenario vm-lifecycle"]
fn a_cold_vm_is_attached_from_saved_values_and_stopped() {
    let inputs = env::var("NVX_VM_LIFECYCLE")
        .expect("NVX_VM_LIFECYCLE is set: run scripts/nvx.py test-microvm --scenario vm-lifecycle");
    let inputs: Inputs = serde_json::from_str(&inputs).expect("the inputs are a JSON object");
    let hypervisor = match inputs.backend.as_str() {
        "kvm" => Hypervisor::Kvm,
        "mshv" => Hypervisor::Mshv,
        "whp" => Hypervisor::Whp,
        backend => panic!("no hypervisor is named {backend}"),
    };
    probe_hypervisor(hypervisor).expect("the backend's hypervisor is usable");
    let timeout = Duration::from_secs_f64(inputs.timeout_seconds);
    let identity = |digest: &str| ArtifactIdentity::sha256(digest).expect("a SHA-256 digest");
    let runtime = Runtime {
        openvmm: inputs.openvmm,
        kernel: inputs.kernel,
        initramfs: inputs.initramfs,
        identity: RuntimeIdentity::new(
            identity(&inputs.openvmm_sha256),
            identity(&inputs.kernel_sha256),
            identity(&inputs.initramfs_sha256),
            CompatibilityIdentity::new("nvx-vm-lifecycle-test").expect("a compatibility identity"),
        ),
        hypervisor,
        cpu_profile: None,
        windows_breakaway: false,
        options: LaunchOptions::default(),
    };
    let image_uuid = Uuid::parse(&inputs.image_uuid).expect("the layer's UUID");
    let config = VmConfig {
        processors: 1,
        memory_mib: 256,
        root: RootConfig::Layered(LayeredRoot {
            image: inputs.image,
            format: RootImage::erofs(Some(image_uuid)),
            gpt_first_partition_bound: None,
            upper: UpperConfig::Scratch {
                path: inputs.scratch,
                journal: Journal::Allowed,
            },
            overlay: OverlayMode::TrustedLowers,
        }),
        view: View {
            hostname: b"nvx-vm-lifecycle".to_vec(),
            machine_id: MachineIdMode::Generated,
        },
        network: None,
        filesystem: None,
        console: ConsoleAttachment::Disconnected,
        boot: Some(Boot {
            command_line: COMMAND_LINE.to_owned(),
            memory_capacity_mib: None,
            capture: None,
        }),
    };

    vm::create_private_dir(&inputs.vm_dir).expect("the VM directory is created private");
    let dir = VmDir::open(&inputs.vm_dir).expect("the VM directory opens");
    let prepared =
        vm::create(&runtime, &config, None, &dir).expect("the public API creates the cold launch");
    // Creation makes the log, and starts nothing that writes the outcome report.
    assert!(prepared.outputs().log.is_file(), "creation makes the log");
    assert!(
        !prepared.outputs().outcome.exists(),
        "no VM runs before the spawn"
    );
    let launched = vm::spawn(prepared).expect("the public API spawns the cold VM");
    let mut connection = launched
        .connect(Instant::now() + timeout)
        .expect("the launched VM accepts its control connection");
    acknowledged(&mut connection, PING, 1, READY, Instant::now() + timeout);
    drop(connection);
    let saved = SavedVm {
        vm_dir: dir,
        endpoint: launched.outputs.endpoint.clone(),
        capability: *launched.capability.as_bytes(),
        process: launched.process.reference().clone(),
        runtime: runtime.identity.clone(),
    };
    let started = Started {
        process: saved.process.clone(),
        outputs: launched.outputs.clone(),
        timeout,
    };
    // The caller keeps only what it saved.
    drop(launched);

    let mut attached = vm::attach(&saved, &runtime, Instant::now() + timeout)
        .expect("the saved values attach the VM");
    assert_eq!(attached.process.reference(), &saved.process);
    acknowledged(
        &mut attached.connection,
        PING,
        2,
        READY,
        Instant::now() + timeout,
    );
    drop(attached);
    let mut attached = vm::attach(&saved, &runtime, Instant::now() + timeout)
        .expect("the saved values attach the VM again, with a new connection");
    acknowledged(
        &mut attached.connection,
        PING,
        3,
        READY,
        Instant::now() + timeout,
    );
    let grace = Instant::now() + timeout;
    let stopped = vm::stop(
        &attached.process,
        &started.outputs,
        |deadline| {
            acknowledged(&mut attached.connection, STOP, 4, STOPPED, deadline);
            true
        },
        grace,
        grace + timeout,
    )
    .expect("the VM's exit is confirmed");
    assert!(
        stopped.self_exited && !stopped.forced,
        "the guest stops the VM without a forced termination: {stopped:?}"
    );
    assert_eq!(
        stopped.report.expect("the outcome report is readable"),
        Some(Report {
            category: Some("success".to_owned()),
            status_code: Some(0),
            teardown_complete: Some(true),
        }),
    );
    assert!(matches!(
        Process::open(&saved.process),
        Ok(Opened::Exited(_))
    ));
}
