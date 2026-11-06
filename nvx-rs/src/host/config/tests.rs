//! Tests of a VM's configuration and of its check, on paths that don't exist, since the check
//! reads nothing on the host.

use std::collections::HashSet;
use std::error::Error as _;
use std::time::Duration;

use super::*;
use crate::activation::MachineIdMode;
use crate::host::filesystem::{Bind, ExportChild, Owner};
use crate::host::network::Access;
use crate::host::runtime::{ArtifactIdentity, GuestPowerAction, LaunchOptions, RuntimeIdentity};
use crate::hypervisor::Hypervisor;
use crate::uuid::Uuid;
use crate::{ErrorCode, ErrorKind};

/// Returns the path of `name` in a directory that doesn't exist.
fn missing(name: &str) -> PathBuf {
    std::env::temp_dir().join("nvx-config-missing").join(name)
}

/// Returns a blockless configuration of one processor and 512 MiB.
fn blockless() -> VmConfig {
    VmConfig {
        processors: 1,
        memory_mib: 512,
        root: RootConfig::Blockless,
        view: View {
            hostname: b"box".to_vec(),
            machine_id: MachineIdMode::Image,
        },
        network: None,
        filesystem: None,
        console: ConsoleAttachment::Listener,
        boot: None,
    }
}

/// Returns a layered root of an EROFS image with a RAM upper of the default cap.
fn erofs() -> LayeredRoot {
    LayeredRoot {
        image: missing("image.erofs"),
        format: RootImage::erofs(None),
        gpt_first_partition_bound: None,
        upper: UpperConfig::Ram {
            max_memory_percent: UpperConfig::DEFAULT_MAX_MEMORY_PERCENT,
        },
        overlay: OverlayMode::TrustedLowers,
    }
}

/// Returns a configuration of `root`, which is otherwise [`blockless`]'s.
fn layered(root: LayeredRoot) -> VmConfig {
    VmConfig {
        root: RootConfig::Layered(root),
        ..blockless()
    }
}

/// Returns a plan that exports one host directory read-only at `target`.
fn export(target: &str) -> ExportPlan {
    ExportPlan {
        children: vec![ExportChild {
            root: missing("export"),
            file: false,
            writable: false,
            denied: Vec::new(),
            denied_identities: Vec::new(),
            allowed: Vec::new(),
            write: Vec::new(),
        }],
        binds: vec![Bind {
            child: 0,
            source: String::new(),
            target: target.to_owned(),
            read_only: true,
            identity: None,
        }],
        owner: Owner::Vmm,
    }
}

#[test]
fn blockless_and_layered_configurations_pass_with_either_console() {
    let gpt = LayeredRoot {
        image: missing("image.gpt"),
        format: RootImage::gpt(Some(Uuid::from_bytes([7; 16]))),
        gpt_first_partition_bound: Some(64 * 1024),
        upper: UpperConfig::Scratch {
            path: missing("scratch.ext4"),
            journal: Journal::Forbidden,
        },
        overlay: OverlayMode::UntrustedLowers,
    };
    let unbounded = LayeredRoot {
        gpt_first_partition_bound: None,
        ..gpt.clone()
    };
    for config in [
        blockless(),
        layered(erofs()),
        layered(gpt),
        layered(unbounded),
    ] {
        for console in [ConsoleAttachment::Listener, ConsoleAttachment::Disconnected] {
            let config = VmConfig {
                console,
                ..config.clone()
            };
            config.check().unwrap();
        }
    }
}

#[test]
fn configurations_pass_with_or_without_a_nic_address_and_a_boot() {
    let networks = [
        None,
        Some(NetworkConfig {
            guest_network: None,
            policy: NetworkPolicy::new(Access::Deny),
        }),
        Some(NetworkConfig {
            guest_network: Some(Ipv4Cidr::parse(DEFAULT_GUEST_NETWORK).unwrap()),
            policy: NetworkPolicy::new(Access::Allow),
        }),
    ];
    let boots = [
        None,
        Some(Boot {
            command_line: String::new(),
            capture: None,
        }),
        Some(Boot {
            command_line: "quiet loglevel=0".to_owned(),
            capture: None,
        }),
    ];
    for config in [blockless(), layered(erofs())] {
        for network in &networks {
            for boot in &boots {
                let config = VmConfig {
                    network: network.clone(),
                    boot: boot.clone(),
                    ..config.clone()
                };
                config.check().unwrap();
            }
        }
    }
}

#[test]
fn a_ram_upper_holds_1_to_90_percent_of_the_memory_and_half_by_default() {
    assert_eq!(UpperConfig::DEFAULT_MAX_MEMORY_PERCENT, 50);
    let ram = |max_memory_percent| {
        layered(LayeredRoot {
            upper: UpperConfig::Ram { max_memory_percent },
            ..erofs()
        })
    };
    for cap in [1, UpperConfig::DEFAULT_MAX_MEMORY_PERCENT, 90] {
        ram(cap).check().unwrap();
    }
    for cap in [0, 91, u8::MAX] {
        let error = ram(cap).check().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        assert!(error.to_string().contains("RAM upper's cap"), "{error}");
    }
}

#[test]
fn each_refusal_names_what_it_refuses() {
    let view = |hostname: &[u8]| VmConfig {
        view: View {
            hostname: hostname.to_vec(),
            machine_id: MachineIdMode::Generated,
        },
        ..blockless()
    };
    // The longest hostname passes.
    view(&[b'h'; MAX_HOSTNAME_BYTES]).check().unwrap();
    let refused = [
        (
            VmConfig {
                processors: 0,
                ..blockless()
            },
            "processor",
        ),
        (
            VmConfig {
                memory_mib: 0,
                ..blockless()
            },
            "memory",
        ),
        (
            layered(LayeredRoot {
                image: PathBuf::new(),
                ..erofs()
            }),
            "no image",
        ),
        (
            layered(LayeredRoot {
                gpt_first_partition_bound: Some(1),
                ..erofs()
            }),
            "EROFS image",
        ),
        (
            layered(LayeredRoot {
                format: RootImage::gpt(None),
                gpt_first_partition_bound: Some(0),
                ..erofs()
            }),
            "first partition is 0",
        ),
        (
            layered(LayeredRoot {
                upper: UpperConfig::Scratch {
                    path: PathBuf::new(),
                    journal: Journal::Allowed,
                },
                ..erofs()
            }),
            "no scratch image",
        ),
        (view(b""), "hostname"),
        (view(&[b'h'; MAX_HOSTNAME_BYTES + 1]), "hostname"),
        (view(b"a\0b"), "hostname"),
    ];
    for (config, refusal) in refused {
        let error = config.check().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        assert!(error.to_string().contains(refusal), "{error}");
    }
}

#[test]
fn an_export_passes_the_check_of_the_targets_that_its_root_reserves() {
    // A blockless root runs the guest's own system from /usr, which a layered root's image holds.
    let usr = export("/usr/local/work");
    // A layered root's runtime stages /.nvx-agent, which a blockless root leaves free.
    let agent = export("/.nvx-agent/work");
    let with = |config: VmConfig, plan: &ExportPlan| VmConfig {
        filesystem: Some(plan.clone()),
        ..config
    };
    with(layered(erofs()), &usr).check().unwrap();
    with(blockless(), &agent).check().unwrap();
    for (config, target) in [
        (with(blockless(), &usr), "/usr"),
        (with(layered(erofs()), &agent), "/.nvx-agent"),
    ] {
        let error = config.check().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        assert_eq!(error.to_string(), "the export plan is invalid");
        let source = error.source().expect("the export's error").to_string();
        assert!(source.contains(target), "{source}");
    }
}

#[test]
fn invariant_settings_have_one_form() {
    let settings = Invariants::new()
        .with("isolation.seccomp", "strict")
        .unwrap()
        .with("limits.pids", "512")
        .unwrap()
        .with("isolation.seccomp", "relaxed")
        .unwrap();
    assert_eq!(settings.len(), 2);
    assert!(!settings.is_empty());
    assert_eq!(settings.get("isolation.seccomp"), Some("relaxed"));
    assert_eq!(settings.get("limits.cpu"), None);
    assert_eq!(
        settings.iter().collect::<Vec<_>>(),
        [("isolation.seccomp", "relaxed"), ("limits.pids", "512")]
    );
    let long_name = "a".repeat(Invariants::MAX_NAME_LEN + 1);
    let long_value = "v".repeat(Invariants::MAX_VALUE_LEN + 1);
    for (name, value) in [
        ("", "value"),
        ("Isolation", "value"),
        ("1st", "value"),
        ("with space", "value"),
        (long_name.as_str(), "value"),
        ("name", long_value.as_str()),
        ("name", "line\nbreak"),
        ("name", "tab\there"),
        ("name", "é"),
    ] {
        let error = Invariants::new().with(name, value).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unsupported, "{name:?} {value:?}");
    }
    assert!(Invariants::new().with("name", "").is_ok());
    let mut full = Invariants::new();
    for index in 0..Invariants::MAX_SETTINGS {
        full = full.with(&format!("setting{index}"), "1").unwrap();
    }
    assert!(full.clone().with("setting0", "2").is_ok());
    assert_eq!(
        full.with("another", "1").unwrap_err().message(),
        "a template holds at most 64 invariant settings"
    );
}

#[test]
fn block_geometries_hold_whole_blocks_of_power_of_two_sizes() {
    let valid = BlockGeometry {
        length: 64 * 1024 * 1024,
        logical_block_size: 512,
        physical_block_size: 4096,
    };
    assert_eq!(valid.problem(), None);
    for (geometry, expected) in [
        (
            BlockGeometry {
                logical_block_size: 256,
                ..valid
            },
            "logical block size 256 is not a power of two of at least 512",
        ),
        (
            BlockGeometry {
                physical_block_size: 256,
                ..valid
            },
            "physical block size 256 is not a power of two of at least its logical block size",
        ),
        (
            BlockGeometry {
                length: 1000,
                ..valid
            },
            "length 1000 is not a nonzero multiple of its logical block size",
        ),
    ] {
        assert_eq!(geometry.problem().as_deref(), Some(expected));
    }
}

/// Returns a fresh capture of 1 processor online of 2, and of 256 MiB of memory of 512.
fn capture() -> CaptureConfig {
    let image = BlockGeometry {
        length: 1024 * 1024 * 1024,
        logical_block_size: 512,
        physical_block_size: 4096,
    };
    CaptureConfig {
        disks: DiskCapture::Fresh {
            image,
            scratch: BlockGeometry {
                length: 64 * 1024 * 1024,
                ..image
            },
        },
        processors: Capacity {
            boot: 1,
            capacity: 2,
        },
        memory: Capacity {
            boot: 256,
            capacity: 512,
        },
        invariants: Invariants::new()
            .with("isolation.seccomp", "strict")
            .unwrap(),
    }
}

#[test]
fn fresh_captures_pass_with_supported_processors_and_aligned_memory() {
    for (processors, memory) in [
        ((1, 1), (1, 128)),
        ((2, 4), (128, 128)),
        ((1, 8), (256, 512)),
        ((8, 8), (4096, 4096)),
    ] {
        let capture = CaptureConfig {
            processors: Capacity {
                boot: processors.0,
                capacity: processors.1,
            },
            memory: Capacity {
                boot: memory.0,
                capacity: memory.1,
            },
            ..capture()
        };
        capture.check().unwrap();
    }
}

#[test]
fn each_refusal_of_a_capture_names_what_it_refuses() {
    type Refusal = fn(&mut CaptureConfig);
    let cases: &[(Refusal, &str)] = &[
        (
            |capture| capture.processors.boot = 3,
            "a capture of 2 processors with 3 online at boot needs 1, 2, 4, or 8 of each",
        ),
        (
            |capture| capture.processors.boot = 0,
            "with 0 online at boot",
        ),
        (
            |capture| capture.processors.capacity = 16,
            "a capture of 16 processors with 1 online at boot",
        ),
        (
            |capture| {
                capture.processors = Capacity {
                    boot: 4,
                    capacity: 2,
                }
            },
            "a capture of 2 processors can't have 4 online at boot",
        ),
        (
            |capture| capture.memory.boot = 0,
            "a capture needs memory at boot",
        ),
        (
            |capture| {
                capture.memory = Capacity {
                    boot: 512,
                    capacity: 384,
                }
            },
            "a capture's memory capacity of 384 MiB is below its 512 MiB at boot",
        ),
        (
            |capture| capture.memory.capacity = 600,
            "a capture's memory capacity of 600 MiB is not a multiple of 128 MiB",
        ),
        (
            |capture| {
                let DiskCapture::Fresh { image, .. } = &mut capture.disks;
                image.logical_block_size = 256;
            },
            "the image disk of a capture is invalid: its logical block size 256",
        ),
        (
            |capture| {
                let DiskCapture::Fresh { scratch, .. } = &mut capture.disks;
                scratch.length = 1000;
            },
            "the scratch disk of a capture is invalid: its length 1000",
        ),
    ];
    for (change, expected) in cases {
        let mut capture = capture();
        change(&mut capture);
        let error = capture.check().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        assert!(error.to_string().contains(expected), "{error}");
    }
}

#[test]
fn the_check_of_a_configuration_checks_its_boots_capture() {
    let booting = |capture: Option<CaptureConfig>| VmConfig {
        boot: Some(Boot {
            command_line: String::new(),
            capture,
        }),
        ..blockless()
    };
    blockless().check().unwrap();
    booting(None).check().unwrap();
    booting(Some(capture())).check().unwrap();
    type Refusal = fn(&mut CaptureConfig);
    let refusals: [Refusal; 2] = [
        |capture| capture.memory.boot = 0,
        |capture| capture.processors.boot = 3,
    ];
    for change in refusals {
        let mut capture = capture();
        change(&mut capture);
        let expected = capture.check().unwrap_err();
        let error = booting(Some(capture)).check().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        assert_eq!(error.to_string(), expected.to_string());
    }
}

/// Returns a runtime of artifacts that don't exist, under this crate's contracts.
fn runtime() -> Runtime {
    let artifact = |digit: char| ArtifactIdentity::sha256(&digit.to_string().repeat(64)).unwrap();
    Runtime {
        openvmm: missing("openvmm"),
        kernel: missing("vmlinux"),
        initramfs: missing("initramfs.cpio.gz"),
        identity: RuntimeIdentity::new(artifact('a'), artifact('b'), artifact('c')),
        hypervisor: Hypervisor::Kvm,
        cpu_profile: None,
        windows_breakaway: false,
        options: LaunchOptions::default(),
    }
}

/// Returns the configuration of a VM that [`capture`] captures: blockless, with a NIC that names
/// no address under a policy that denies everything, a listener on its console, no export, and
/// a boot of one token.
fn captured() -> VmConfig {
    VmConfig {
        network: Some(NetworkConfig {
            guest_network: None,
            policy: NetworkPolicy::new(Access::Deny),
        }),
        boot: Some(Boot {
            command_line: "quiet".to_owned(),
            capture: Some(capture()),
        }),
        ..blockless()
    }
}

#[test]
fn equal_captures_have_one_identity_in_one_text() {
    let identity = capture().identity(&captured(), &runtime());
    assert_eq!(identity, capture().identity(&captured(), &runtime()));
    // A NIC that names no address takes the default one at a capture's cold boot.
    let explicit = VmConfig {
        network: Some(NetworkConfig {
            guest_network: Some(Ipv4Cidr::parse(DEFAULT_GUEST_NETWORK).unwrap()),
            policy: NetworkPolicy::new(Access::Deny),
        }),
        ..captured()
    };
    assert_eq!(capture().identity(&explicit, &runtime()), identity);

    let text = identity.to_string();
    assert_eq!(text.len(), 64);
    assert!(
        text.bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    assert_eq!(format!("{identity:?}"), format!("CaptureIdentity({text})"));
    let json = serde_json::to_string(&identity).unwrap();
    assert_eq!(json, format!("\"{text}\""));
    assert_eq!(
        serde_json::from_str::<CaptureIdentity>(&json).unwrap(),
        identity
    );
    assert_eq!(
        serde_json::from_str::<CaptureIdentity>(&json.to_uppercase()).unwrap(),
        identity
    );
    for invalid in ["\"\"", "\"abc\"", "64"] {
        assert!(
            serde_json::from_str::<CaptureIdentity>(invalid).is_err(),
            "{invalid}"
        );
    }
}

/// A change to the inputs of a capture's identity.
type Change = fn(&mut CaptureConfig, &mut VmConfig, &mut Runtime);

#[test]
fn each_input_that_identifies_a_capture_changes_its_identity() {
    let changes: &[(&str, Change)] = &[
        ("the OpenVMM", |_, _, runtime| {
            runtime.identity.openvmm = ArtifactIdentity::new("openvmm-2").unwrap();
        }),
        ("the kernel", |_, _, runtime| {
            runtime.identity.kernel = ArtifactIdentity::new("vmlinux-2").unwrap();
        }),
        ("the initramfs", |_, _, runtime| {
            runtime.identity.initramfs = ArtifactIdentity::new("initramfs-2").unwrap();
        }),
        ("the boot contract", |_, _, runtime| {
            runtime.identity.boot_contract += 1
        }),
        ("the control contract", |_, _, runtime| {
            runtime.identity.control_contract.push('2');
        }),
        ("the control session", |_, _, runtime| {
            runtime.identity.control_session += 1
        }),
        ("the hypervisor", |_, _, runtime| {
            runtime.hypervisor = Hypervisor::Mshv
        }),
        ("the CPU profile", |_, _, runtime| {
            runtime.cpu_profile = Some("auto".to_owned());
        }),
        ("the image's length", |capture, _, _| {
            let DiskCapture::Fresh { image, .. } = &mut capture.disks;
            image.length *= 2;
        }),
        ("the image's logical blocks", |capture, _, _| {
            let DiskCapture::Fresh { image, .. } = &mut capture.disks;
            image.logical_block_size = 4096;
        }),
        ("the image's physical blocks", |capture, _, _| {
            let DiskCapture::Fresh { image, .. } = &mut capture.disks;
            image.physical_block_size = 512;
        }),
        ("the scratch's length", |capture, _, _| {
            let DiskCapture::Fresh { scratch, .. } = &mut capture.disks;
            scratch.length *= 2;
        }),
        ("the scratch's logical blocks", |capture, _, _| {
            let DiskCapture::Fresh { scratch, .. } = &mut capture.disks;
            scratch.logical_block_size = 4096;
        }),
        ("the scratch's physical blocks", |capture, _, _| {
            let DiskCapture::Fresh { scratch, .. } = &mut capture.disks;
            scratch.physical_block_size = 512;
        }),
        ("the processors at boot", |capture, _, _| {
            capture.processors.boot = 2
        }),
        ("the processor capacity", |capture, _, _| {
            capture.processors.capacity = 4
        }),
        ("the memory at boot", |capture, _, _| {
            capture.memory.boot = 128
        }),
        ("the memory capacity", |capture, _, _| {
            capture.memory.capacity = 640
        }),
        ("another setting", |capture, _, _| {
            capture.invariants = capture
                .invariants
                .clone()
                .with("limits.pids", "512")
                .unwrap();
        }),
        ("a setting's value", |capture, _, _| {
            capture.invariants = capture
                .invariants
                .clone()
                .with("isolation.seccomp", "x")
                .unwrap();
        }),
        ("a setting's name", |capture, _, _| {
            capture.invariants = Invariants::new().with("isolation.mode", "strict").unwrap();
        }),
        ("no NIC", |_, config, _| config.network = None),
        ("the NIC's address", |_, config, _| {
            config.network.as_mut().unwrap().guest_network =
                Some(Ipv4Cidr::parse("10.1.0.2/24").unwrap());
        }),
        ("the console", |_, config, _| {
            config.console = ConsoleAttachment::Disconnected;
        }),
        ("an export", |_, config, _| {
            config.filesystem = Some(export("/mnt/work"));
        }),
        ("the command line", |_, config, _| {
            config
                .boot
                .as_mut()
                .unwrap()
                .command_line
                .push_str(" debug");
        }),
        ("no boot", |_, config, _| config.boot = None),
    ];
    let base = capture().identity(&captured(), &runtime());
    let mut identities = HashSet::from([base]);
    for (what, change) in changes {
        let (mut capture, mut config, mut runtime) = (capture(), captured(), runtime());
        change(&mut capture, &mut config, &mut runtime);
        assert!(
            identities.insert(capture.identity(&config, &runtime)),
            "{what}"
        );
    }
}

#[test]
fn what_each_launch_or_clone_sets_leaves_the_identity_alone() {
    let changes: &[(&str, Change)] = &[
        ("the paths of the runtime", |_, _, runtime| {
            runtime.openvmm = missing("other-openvmm");
            runtime.kernel = missing("other-vmlinux");
            runtime.initramfs = missing("other-initramfs");
        }),
        ("the breakaway", |_, _, runtime| {
            runtime.windows_breakaway = true
        }),
        ("the launch options", |_, _, runtime| {
            runtime.options = LaunchOptions {
                guest_shutdown: GuestPowerAction::Halt,
                control_auth_timeout: Some(Duration::from_secs(9)),
            };
        }),
        ("the view", |_, config, _| {
            config.view = View {
                hostname: b"clone".to_vec(),
                machine_id: MachineIdMode::Image,
            };
        }),
        ("the policy", |_, config, _| {
            config.network.as_mut().unwrap().policy = NetworkPolicy::new(Access::Allow);
        }),
        ("the VM's processors and memory", |_, config, _| {
            config.processors = 2;
            config.memory_mib = 1024;
        }),
        // The VM of a supported capture is blockless, and each clone attaches its own root.
        ("a layered root", |_, config, _| {
            config.root = RootConfig::Layered(erofs())
        }),
        (
            "a layered root's format, identity, bound, upper and overlay",
            |_, config, _| {
                config.root = RootConfig::Layered(LayeredRoot {
                    format: RootImage::gpt(Some(Uuid::from_bytes([7; 16]))),
                    gpt_first_partition_bound: Some(64 * 1024),
                    upper: UpperConfig::Scratch {
                        path: missing("scratch.ext4"),
                        journal: Journal::Forbidden,
                    },
                    overlay: OverlayMode::UntrustedLowers,
                    ..erofs()
                });
            },
        ),
    ];
    let base = capture().identity(&captured(), &runtime());
    for (what, change) in changes {
        let (mut capture, mut config, mut runtime) = (capture(), captured(), runtime());
        change(&mut capture, &mut config, &mut runtime);
        assert_eq!(capture.identity(&config, &runtime), base, "{what}");
    }
    // Of an export, only that there is one: not its paths or targets.
    let with_export = |target| VmConfig {
        filesystem: Some(export(target)),
        ..captured()
    };
    assert_eq!(
        capture().identity(&with_export("/mnt/a"), &runtime()),
        capture().identity(&with_export("/mnt/b"), &runtime())
    );
}
