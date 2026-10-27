//! The host program's captures of templates, without a hypervisor: the capture that
//! `runtime_adapter::capture` makes of what the host program asks of a template, with its class,
//! and the creations that the host program refuses before it creates anything. The VM tests of
//! `tests/vm_template.rs` capture templates on a real hypervisor.
#![cfg(feature = "host")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use nvx::Hypervisor;
use nvx::host::artifacts::Artifacts;
use nvx::host::sandbox::SandboxNetwork;
use nvx::host::sandbox::distro::DISTRO_POLICY_REVISION;
use nvx::host::store::{ArtifactIdentity, RuntimeIdentity};
use nvx::protocol::root::{ImageFormat, RootImage};
use nvx::uuid::Uuid;
use runtime_adapter::capture::{
    DEFAULT_BOOT_PROCESSORS, DEFAULT_CPU_PROFILE, DEFAULT_PROCESSORS, DEFAULT_SCRATCH_LENGTH,
    LOGICAL_BLOCK_SIZE, Request, geometry,
};
use runtime_adapter::layered::read_network;
use runtime_adapter::runtime::Runtime;
use runtime_adapter::staged::Staged;
use serde_json::{Value, json};

/// Bytes in a mebibyte.
const MIB: u64 = 1024 * 1024;

/// Returns a directory of the test `name`, empty, below Cargo's temporary directory.
fn directory(name: &str) -> PathBuf {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    directory
}

/// Returns the identity that the tests give their placeholders.
fn identity() -> Uuid {
    Uuid::parse("0a490905-41be-ed5f-984e-63eb7e5e20b1").unwrap()
}

/// Returns the serde form of a network with egress allowed, and ingress and host-loopback access
/// denied, which a template takes.
fn egress_network() -> Value {
    json!({
        "guestNetwork": "192.0.2.2/24",
        "egress": "allow",
        "allow": {"name": "egress.allow", "rules": []},
        "deny": {"name": "egress.deny", "rules": []},
        "ingress": "deny",
        "hostLoopback": "deny",
        "hostLoopbackForwards": [],
        "proxy": null,
    })
}

/// Returns the runtime of NVX's artifacts in `directory`, which the tests never boot, with the
/// runtime identity of the adapter's VMs.
fn runtime(directory: &Path) -> Runtime {
    let identity = |text: &str| ArtifactIdentity::new(text).unwrap();
    Runtime {
        artifacts: Artifacts::new(
            directory.join("openvmm"),
            directory.join("vmlinux"),
            directory.join("initramfs.cpio.gz"),
        ),
        identity: RuntimeIdentity::new(
            identity("sha256:openvmm"),
            identity("sha256:kernel"),
            identity("sha256:initramfs+runtime-adapter-guest/build"),
        ),
    }
}

/// Returns the request of a template of the placeholder `distro`, with the host program's defaults
/// and `network`.
fn template(distro: &Path, network: Option<SandboxNetwork>) -> Request {
    Request {
        distro: distro.to_owned(),
        format: ImageFormat::Erofs,
        identity: identity(),
        hypervisor: Hypervisor::Kvm,
        cpu_profile: DEFAULT_CPU_PROFILE.to_owned(),
        processors: DEFAULT_PROCESSORS,
        boot_processors: DEFAULT_BOOT_PROCESSORS,
        memory: 256 * MIB,
        memory_capacity: 256 * MIB,
        scratch_length: DEFAULT_SCRATCH_LENGTH,
        network,
    }
}

/// Returns the physical block size that OpenVMM presents for a sandbox block device whose file is,
/// or is in, `path` on this host: the file system's preferred size for I/O on Linux, and 4096 on
/// Windows.
fn physical_block_size(path: &Path) -> u32 {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt as _;
        u32::try_from(fs::metadata(path).unwrap().blksize()).unwrap()
    }
    #[cfg(windows)]
    {
        let _ = path;
        4096
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = path;
        LOGICAL_BLOCK_SIZE
    }
}

#[test]
fn a_capture_takes_its_class_and_inputs_from_the_request() {
    let directory = directory("template-capture-request");
    let distro = directory.join("placeholder.erofs");
    fs::write(&distro, vec![0; 2 * MIB as usize]).unwrap();
    let runtime = runtime(&directory);
    let initramfs = directory.join("template.cpio.gz");
    let network: SandboxNetwork = serde_json::from_value(egress_network()).unwrap();
    let request = template(&distro, Some(network.clone()));
    let staged = Staged::stage(&distro, ImageFormat::Erofs, identity(), None).unwrap();
    let capture = request
        .capture(
            staged.distro.binding.clone(),
            &runtime,
            "build",
            &initramfs,
            &directory,
        )
        .unwrap();

    // The adapter's plan of the placeholder, as the host program stages it, with the network,
    // over RAM, which no template keeps.
    let adapter = &capture.adapter;
    assert_eq!(adapter.distro().path(), staged.distro.path);
    assert_eq!(
        adapter.distro().image(),
        RootImage {
            format: ImageFormat::Erofs,
            identity: Some(identity()),
        }
    );
    assert_eq!(adapter.upper().scratch_path(), None);
    assert!(adapter.settings().is_none());
    assert_eq!(adapter.network(), Some(&network));
    assert_eq!(capture.binding, staged.distro.binding);
    assert_eq!(capture.policy_revision, DISTRO_POLICY_REVISION);
    assert_eq!(capture.artifacts.openvmm, runtime.artifacts.openvmm);
    assert_eq!(capture.artifacts.kernel, runtime.artifacts.kernel);
    assert_eq!(capture.artifacts.initrd, initramfs);

    // The class: the runtime, the guest agent's build, the machine that the request asks for,
    // with its processors online at boot, the geometries that OpenVMM presents, and the network.
    let class = &capture.class;
    assert_eq!(class.runtime, runtime.identity);
    assert_eq!(class.guest_build.as_str(), "runtime-adapter-guest/build");
    assert_eq!(class.hypervisor, Hypervisor::Kvm);
    assert_eq!(class.cpu_profile, "auto");
    assert_eq!((class.processors, class.boot_processors), (1, 1));
    assert_eq!(
        (class.memory, class.memory_capacity),
        (256 * MIB, 256 * MIB)
    );
    assert_eq!(class.distro.length, 2 * MIB);
    assert_eq!(class.distro.logical_block_size, LOGICAL_BLOCK_SIZE);
    assert_eq!(
        class.distro.physical_block_size,
        physical_block_size(&distro)
    );
    assert_eq!(class.scratch.length, 64 * MIB);
    assert_eq!(class.scratch.logical_block_size, LOGICAL_BLOCK_SIZE);
    assert_eq!(
        class.scratch.physical_block_size,
        physical_block_size(&directory)
    );
    assert_eq!(class.network.as_ref(), Some(&network));
    assert!(class.invariants.is_empty());
    let with_nic = class.identity().unwrap();

    // A template keeps the processors online at boot that the request names, 1 by default, the
    // fewest that its clones take, whatever its capacity, and the network names the class: one
    // without a network is another.
    let mut more = template(&distro, None);
    more.processors = 4;
    more.memory_capacity = 512 * MIB;
    let capture = more
        .capture(
            staged.distro.binding.clone(),
            &runtime,
            "build",
            &initramfs,
            &directory,
        )
        .unwrap();
    assert_eq!(
        (capture.class.processors, capture.class.boot_processors),
        (4, 1)
    );
    assert_eq!(capture.class.memory_capacity, 512 * MIB);
    assert_eq!(capture.adapter.network(), None);
    assert_eq!(capture.class.network, None);
    assert_ne!(capture.class.identity().unwrap(), with_nic);
    let fewer = capture.class.identity().unwrap();
    more.boot_processors = 2;
    let capture = more
        .capture(
            staged.distro.binding,
            &runtime,
            "build",
            &initramfs,
            &directory,
        )
        .unwrap();
    assert_eq!(
        (capture.class.processors, capture.class.boot_processors),
        (4, 2)
    );
    assert_ne!(capture.class.identity().unwrap(), fewer);

    // OpenVMM presents logical blocks of 512 bytes.
    let presented = geometry(&distro, 4096).unwrap();
    assert_eq!(presented.length, 4096);
    assert_eq!(presented.logical_block_size, 512);
    assert!(presented.physical_block_size >= 512);
    assert!(presented.physical_block_size.is_power_of_two());
}

#[test]
fn the_host_program_refuses_a_template_before_it_creates_anything() {
    let directory = directory("template-capture-command");
    let distro = directory.join("placeholder.erofs");
    fs::write(&distro, vec![0; 4096]).unwrap();
    let store = directory.join("templates");
    // Without NVX's artifacts, a creation fails once it has read its network.
    let create = |network: Option<&Path>| -> Output {
        let mut image = std::ffi::OsString::from("erofs:");
        image.push(&distro);
        let mut command = Command::new(env!("CARGO_BIN_EXE_runtime-adapter-host"));
        command
            .arg("create")
            .arg(&store)
            .args(["--guest-agent", "agent", "--hypervisor", "kvm", "--distro"])
            .arg(image)
            .args(["--distro-identity", &identity().to_string()]);
        if let Some(network) = network {
            command.arg("--network").arg(network);
        }
        command.env_remove(Artifacts::ENV_DIR).output().unwrap()
    };
    let failed = |output: Output| {
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        // Nothing was created: not even the store.
        assert!(!store.exists(), "{output:?}");
        String::from_utf8_lossy(&output.stderr).into_owned()
    };

    // A network that the plan refuses, and one that cannot be read, fail the creation first.
    let network = directory.join("network.json");
    let mut proxy = egress_network();
    proxy["proxy"] = json!({"port": 3128, "scheme": "http"});
    fs::write(&network, proxy.to_string()).unwrap();
    let stderr = failed(create(Some(&network)));
    let refused = format!(
        "the network {} is refused: a network proxy needs egress denied by default",
        network.display()
    );
    assert!(stderr.contains(&refused), "{stderr}");
    let missing = directory.join("missing.json");
    let stderr = failed(create(Some(&missing)));
    let unreadable = format!("cannot read the network {}", missing.display());
    assert!(stderr.contains(&unreadable), "{stderr}");

    // A network that the plan takes passes, as does no network, and the creation goes on to
    // need NVX's artifacts.
    fs::write(&network, egress_network().to_string()).unwrap();
    read_network(&network).unwrap();
    for network in [Some(network.as_path()), None] {
        let stderr = failed(create(network));
        assert!(
            stderr.contains(&format!("set {} to NVX's artifacts", Artifacts::ENV_DIR)),
            "{stderr}"
        );
    }
}
