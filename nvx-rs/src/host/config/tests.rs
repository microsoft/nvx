//! Tests of a VM's configuration and of its check, on paths that don't exist, since the check
//! reads nothing on the host.

use std::collections::HashSet;
use std::error::Error as _;
use std::time::Duration;

use super::*;
use crate::activation::MachineIdMode;
use crate::host::artifacts::Artifacts;
use crate::host::filesystem::{Bind, ExportChild, Owner};
use crate::host::launch::LaunchSpec;
use crate::host::network::Access;
use crate::host::runtime::{ArtifactIdentity, GuestPowerAction, LaunchOptions, RuntimeIdentity};
use crate::host::sandbox::{SandboxDistro, SandboxPlan, SandboxSettings, SandboxUpper};
use crate::hypervisor::Hypervisor;
use crate::protocol::{cmdline, parse_host_mapping_count};
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

impl VmConfig {
    fn legacy_command_line(&self) -> String {
        let hostname = String::from_utf8(self.view.hostname.clone()).unwrap();
        match &self.root {
            RootConfig::Blockless => {
                let mut spec = LaunchSpec::new(
                    Artifacts::new("/openvmm", "/vmlinux", "/initramfs"),
                    Hypervisor::Kvm,
                );
                spec.hostname = hostname;
                spec.filesystem = self.filesystem.clone();
                spec.kernel_command_line().unwrap()
            }
            RootConfig::Layered(root) => {
                let upper = match &root.upper {
                    UpperConfig::Scratch { path, .. } => SandboxUpper::scratch_at(path.clone()),
                    UpperConfig::Ram { .. } => SandboxUpper::RAM,
                };
                SandboxPlan::new(
                    SandboxDistro::at(root.image.clone(), root.format),
                    upper,
                    SandboxSettings {
                        hostname,
                        ..SandboxSettings::default()
                    },
                )
                .unwrap()
                .with_export(self.filesystem.clone())
                .unwrap()
                .kernel_command_line("")
                .unwrap()
            }
        }
    }
}

#[test]
fn cold_command_lines_keep_the_old_root_view_and_mapping_values() {
    let mut configs = vec![blockless()];
    let uppers = [
        UpperConfig::Scratch {
            path: missing("scratch.ext4"),
            journal: Journal::Allowed,
        },
        UpperConfig::Scratch {
            path: missing("scratch.ext4"),
            journal: Journal::Forbidden,
        },
        UpperConfig::Ram {
            max_memory_percent: 1,
        },
        UpperConfig::Ram {
            max_memory_percent: 90,
        },
    ];
    for format in [ImageFormat::Erofs, ImageFormat::Gpt] {
        for identity in [None, Some(Uuid::from_bytes([7; 16]))] {
            for overlay in [OverlayMode::TrustedLowers, OverlayMode::UntrustedLowers] {
                for upper in &uppers {
                    for bound in [None, Some(64 * 1024)] {
                        if format == ImageFormat::Erofs && bound.is_some() {
                            continue;
                        }
                        configs.push(layered(LayeredRoot {
                            format: RootImage { format, identity },
                            gpt_first_partition_bound: bound,
                            upper: upper.clone(),
                            overlay,
                            ..erofs()
                        }));
                    }
                }
            }
        }
    }
    let mut mappings = export("/work");
    mappings.binds.push(Bind {
        target: "/zother".to_owned(),
        ..mappings.binds[0].clone()
    });
    for config in configs {
        for filesystem in [None, Some(mappings.clone())] {
            for machine_id in [MachineIdMode::Image, MachineIdMode::Generated] {
                let mut config = VmConfig {
                    filesystem: filesystem.clone(),
                    boot: Some(Boot {
                        command_line: String::new(),
                        capture: None,
                    }),
                    ..config.clone()
                };
                config.view.machine_id = machine_id;
                let old = config.legacy_command_line();
                let actual = config.kernel_command_line().unwrap();
                let old_root = RootLayout::parse(old.as_bytes()).unwrap();
                let root = RootLayout::parse(actual.as_bytes()).unwrap();
                assert_eq!(
                    root.map(|root| (root.image, root.upper)),
                    old_root.map(|root| (root.image, root.upper))
                );
                if let Some(old_root) = old_root {
                    assert!(actual.starts_with(&old_root.tokens().join(" ")));
                }
                if let RootConfig::Layered(configured) = &config.root {
                    let root = root.unwrap();
                    assert_eq!(root.overlay, Some(configured.overlay));
                    assert_eq!(
                        root.gpt_first_partition_bound,
                        configured.gpt_first_partition_bound
                    );
                    match configured.upper {
                        UpperConfig::Scratch { journal, .. } => {
                            assert_eq!(root.journal, Some(journal));
                            assert_eq!(root.max_memory_percent, None);
                        }
                        UpperConfig::Ram { max_memory_percent } => {
                            assert_eq!(root.journal, None);
                            assert_eq!(root.max_memory_percent, Some(max_memory_percent));
                        }
                    }
                }
                let decoded = view::parse(actual.as_bytes()).unwrap().unwrap();
                assert_eq!(decoded, config.view);
                let old_hostname = cmdline::init_tokens(old.as_bytes())
                    .find(|token| matches!(token.name, b"hostname" | b"nvx_hostname"))
                    .unwrap()
                    .value
                    .unwrap();
                assert_eq!(decoded.hostname, old_hostname);
                let count = parse_host_mapping_count(actual.as_bytes()).unwrap();
                assert_eq!(count, parse_host_mapping_count(old.as_bytes()).unwrap());
                assert_eq!(count, if filesystem.is_some() { 2 } else { 0 });
                let names: Vec<_> = cmdline::init_tokens(actual.as_bytes())
                    .skip(root.map_or(0, |root| root.tokens().len()))
                    .map(|token| token.name)
                    .collect();
                let mut expected = vec![b"nvx_hostname".as_slice(), b"nvx_machine_id"];
                if filesystem.is_some() {
                    expected.push(b"nvx_maps");
                }
                assert_eq!(names, expected);
            }
        }
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

#[test]
fn caller_tokens_follow_rendered_tokens_without_any_rewriting() {
    let caller = " \tquiet nvx_entrypoint=/bin/work nvx_arg=\"x y\" nvx_share=0,/work\r\n ";
    let mut config = VmConfig {
        boot: Some(Boot {
            command_line: String::new(),
            capture: None,
        }),
        ..blockless()
    };
    let rendered = config.kernel_command_line().unwrap();
    assert!(!rendered.ends_with(' '));
    config.boot.as_mut().unwrap().command_line = caller.to_owned();
    assert_eq!(
        config.kernel_command_line().unwrap(),
        format!("{rendered} {caller}")
    );
}

#[test]
fn only_a_checked_cold_boot_has_a_kernel_command_line() {
    let config = blockless();
    let error = config.kernel_command_line().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    assert!(error.to_string().contains("Boot"), "{error}");
    let config = VmConfig {
        processors: 0,
        boot: Some(Boot {
            command_line: String::new(),
            capture: None,
        }),
        ..config
    };
    let error = config.kernel_command_line().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    assert!(error.to_string().contains("processor"), "{error}");
}

#[test]
fn caller_tokens_cannot_replace_rendered_tokens_under_kernel_name_matching() {
    let reserved = [
        "nvx_layer",
        "nvx_scratch",
        "nvx_overlay_upper",
        "nvx_overlay_mode",
        "nvx_scratch_journal",
        "nvx_first_partition_bound",
        "nvx_ram_upper_percent",
        "nvx_hostname",
        "nvx_machine_id",
        "nvx_maps",
        "virtfs_dir",
        "virtfs_tag",
        "virtfs_mode",
        "virtfs_aggregate",
        "virtfs_future_option",
        "virtfs_",
    ];
    for name in reserved {
        let underscores = name.bytes().filter(|&byte| byte == b'_').count();
        for mask in 0..(1 << underscores) {
            let mut index = 0;
            let spelling: String = name
                .chars()
                .map(|character| {
                    if character != '_' {
                        return character;
                    }
                    let dash = mask & (1 << index) != 0;
                    index += 1;
                    if dash { '-' } else { '_' }
                })
                .collect();
            for command_line in [
                spelling.clone(),
                format!("{spelling}=value"),
                format!("quiet \"{spelling}=value\""),
                format!("quiet\u{a0}{spelling}=value"),
                format!("-- {spelling}=value"),
                format!("other=\"value {spelling}=value\""),
            ] {
                for config in [blockless(), layered(erofs())] {
                    let config = VmConfig {
                        boot: Some(Boot {
                            command_line: command_line.clone(),
                            capture: None,
                        }),
                        ..config
                    };
                    for error in [
                        config.check().unwrap_err(),
                        config.kernel_command_line().unwrap_err(),
                    ] {
                        assert_eq!(error.kind(), ErrorKind::Unsupported, "{command_line:?}");
                        assert!(
                            error.to_string().contains("Boot.command_line"),
                            "{command_line:?}: {error}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn caller_owned_guest_tokens_and_noncolliding_names_stay_unchanged() {
    for command_line in [
        "nvx_entrypoint=/bin/work nvx_arg=\"x y\" nvx_share=0,/work",
        "nvx-entrypoint=/bin/work nvx-arg=x nvx-share=0,/work",
        "nvx_sandbox=1 nvx_lifecycle=one-shot nvx_workload_account=1",
        "quiet tsc=reliable hostname=caller root=/dev/custom",
        "nvx_maps_extra=1 NVX_MAPS=1 virtfs=1 virtfsx_name=1 other_virtfs_dir=1",
        "other=nvx_maps=1 -- nvx_arg=nvx_hostname",
    ] {
        let mut config = VmConfig {
            boot: Some(Boot {
                command_line: String::new(),
                capture: None,
            }),
            ..blockless()
        };
        let rendered = config.kernel_command_line().unwrap();
        config.boot.as_mut().unwrap().command_line = command_line.to_owned();
        config.check().unwrap();
        assert_eq!(
            config.kernel_command_line().unwrap(),
            format!("{rendered} {command_line}")
        );
    }
}

#[test]
fn mapping_table_is_the_exact_export_projection_counted_by_the_boot_token() {
    let mut plan = export("/work");
    plan.children.push(ExportChild {
        root: missing("writable-export"),
        writable: true,
        ..plan.children[0].clone()
    });
    plan.binds.push(Bind {
        child: 1,
        source: String::new(),
        target: "/zother".to_owned(),
        read_only: false,
        identity: None,
    });
    plan.binds.push(Bind {
        source: "nested/file".to_owned(),
        target: "/work/nested/file".to_owned(),
        ..plan.binds[0].clone()
    });
    let mut config = VmConfig {
        filesystem: Some(plan.clone()),
        boot: Some(Boot {
            command_line: String::new(),
            capture: None,
        }),
        ..blockless()
    };
    let table = config.mapping_table();
    assert_eq!(table, plan.guest_table());
    assert_eq!(
        table,
        vec![
            crate::protocol::maps::HostMap {
                source: "0".to_owned(),
                target: "/work".to_owned(),
                read_only: true,
            },
            crate::protocol::maps::HostMap {
                source: "1".to_owned(),
                target: "/zother".to_owned(),
                read_only: false,
            },
            crate::protocol::maps::HostMap {
                source: "0/nested/file".to_owned(),
                target: "/work/nested/file".to_owned(),
                read_only: true,
            },
        ]
    );
    let command_line = config.kernel_command_line().unwrap();
    assert_eq!(
        parse_host_mapping_count(command_line.as_bytes()).unwrap(),
        u32::try_from(table.len()).unwrap()
    );
    config.processors = 0;
    assert!(config.check().is_err());
    assert_eq!(config.mapping_table(), table);
    config.processors = 1;
    config.filesystem = None;
    assert!(config.mapping_table().is_empty());
    let command_line = config.kernel_command_line().unwrap();
    assert!(!cmdline::init_tokens(command_line.as_bytes()).any(|token| token.name == b"nvx_maps"));
}

#[test]
fn public_maps_codec_keeps_the_protocol_root_reexports_usable() {
    let table: Vec<crate::protocol::HostMap> = export("/work").guest_table();
    let payloads = crate::protocol::maps::encode_maps_payloads(&table, 1024).unwrap();
    let decoded: crate::protocol::MapsPayload =
        crate::protocol::maps::decode_maps_payload(&payloads[0]).unwrap();
    assert_eq!(decoded.maps, table);
    let payloads = crate::protocol::encode_maps_payloads(&decoded.maps, 1024).unwrap();
    let decoded: crate::protocol::maps::MapsPayload =
        crate::protocol::decode_maps_payload(&payloads[0]).unwrap();
    assert_eq!(decoded.maps, table);
}
