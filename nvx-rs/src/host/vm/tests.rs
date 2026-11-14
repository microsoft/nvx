use std::ffi::OsStr;
use std::fs;
#[cfg(any(target_os = "linux", windows))]
use std::process::{Command, Stdio};
#[cfg(any(target_os = "linux", windows))]
use std::time::{Duration, Instant};

use super::*;
use crate::ErrorKind;
use crate::guest_restore::{Network, Publication};
use crate::host::config::{
    Boot, Capacity, DEFAULT_GUEST_NETWORK, Invariants, LayeredRoot, PROCESS_COMMAND_LINE_LIMIT,
    RootConfig, UpperConfig, process_command_line_units,
};
use crate::host::filesystem::{Bind, ExportChild, ExportPlan, Owner};
use crate::host::network::{
    Access, EgressRule, HostLoopback, NetworkPolicy, NetworkProxy, Protocol, ProxyScheme,
};
#[cfg(any(target_os = "linux", windows))]
use crate::host::process::Opened;
use crate::host::runtime::{
    ArtifactIdentity, CompatibilityIdentity, Hypervisor, LaunchOptions, RuntimeIdentity,
};
use crate::host::snapshot::{DESCRIPTION_FILE, KEY_FILE};
use crate::host::vm_directory::{LOG, OUTCOME, RESTORE_READY, tests::vm_dir};
use crate::net::Ipv4Cidr;
use crate::protocol::root::{Journal, OverlayMode, RootImage};
use crate::protocol::view::{MachineIdMode, View};
use crate::uuid::Uuid;

#[cfg(any(target_os = "linux", windows))]
mod reattach;

#[cfg(all(feature = "cli", any(target_os = "linux", windows)))]
impl Launched {
    /// Spawns `command`, or the stand-in VMM without one, as a cold launch in `dir` spawns
    /// OpenVMM, with these outputs and restore-ready listener.
    pub(crate) fn stand_in(
        dir: &VmDir,
        outputs: VmOutputs,
        ready: Option<ReadyListener>,
        command: Option<(PathBuf, Vec<OsString>)>,
    ) -> Self {
        let mut prepared = stand_in(dir);
        prepared.outputs = outputs;
        prepared.ready = ready;
        if let Some((program, arguments)) = command {
            prepared.program = program;
            prepared.arguments = arguments;
        }
        spawn(prepared).unwrap()
    }
}

#[cfg(all(feature = "cli", any(target_os = "linux", windows)))]
impl Prepared {
    /// Runs `program` with `arguments` in place of OpenVMM, with the rest of the checked launch:
    /// its capability, outputs, log, inherited resources, and restore-ready listener.
    pub(crate) fn run_instead(mut self, program: PathBuf, arguments: Vec<OsString>) -> Self {
        self.program = program;
        self.arguments = arguments;
        self
    }
}

#[test]
fn the_reserved_names_are_the_documented_ones_and_leave_the_openvmm_prefix_to_openvmm() {
    let names = [
        LOG,
        OUTCOME,
        CONTROL,
        CONSOLE,
        RESTORE_READY,
        OPENVMM,
        KEY_FILE,
        DESCRIPTION_FILE,
        CAPTURE_IMAGE,
        CAPTURE_SCRATCH,
    ];
    assert_eq!(
        names,
        [
            "openvmm.log",
            "outcome.json",
            "control.sock",
            "console.sock",
            "restore-ready.sock",
            "openvmm",
            "key.bin",
            "snapshot.json",
            "capture-image.raw",
            "capture-scratch.raw",
        ]
    );
    for name in names {
        assert!(!name.starts_with(".openvmm"), "{name}");
    }
}

#[test]
fn a_capability_gives_its_bytes_and_its_debug_hides_them() {
    let bytes: [u8; CAPABILITY_LEN] = std::array::from_fn(|index| u8::try_from(index + 1).unwrap());
    let capability = Capability(bytes);
    assert_eq!(capability.as_bytes(), &bytes);
    assert_eq!(format!("{capability:?}"), "Capability { .. }");
    assert_eq!(format!("{capability:#?}"), "Capability { .. }");
}

fn cold_config() -> VmConfig {
    VmConfig {
        processors: 1,
        memory_mib: 256,
        root: RootConfig::Blockless,
        view: View {
            hostname: b"box".to_vec(),
            machine_id: MachineIdMode::Image,
        },
        network: None,
        filesystem: None,
        console: ConsoleAttachment::Listener,
        boot: Some(Boot {
            memory_capacity_mib: None,
            command_line: "nvx_entrypoint=/bin/sh nvx_arg=-c nvx_arg=exit".to_owned(),
            capture: None,
        }),
    }
}

/// Returns the compatibility identity of the runtimes of the tests.
fn test_compatibility() -> CompatibilityIdentity {
    CompatibilityIdentity::new("test-runtime").unwrap()
}

fn cold_runtime() -> Runtime {
    let identity = ArtifactIdentity::new("test-runtime").unwrap();
    Runtime {
        openvmm: absolute(Path::new("missing-openvmm")).unwrap(),
        kernel: absolute(Path::new("missing-kernel")).unwrap(),
        initramfs: absolute(Path::new("missing-initramfs")).unwrap(),
        identity: RuntimeIdentity::new(
            identity.clone(),
            identity.clone(),
            identity,
            test_compatibility(),
        ),
        hypervisor: if cfg!(windows) {
            Hypervisor::Whp
        } else {
            Hypervisor::Mshv
        },
        cpu_profile: None,
        windows_breakaway: false,
        options: LaunchOptions::default(),
    }
}

fn capture_config(_vm: &VmDir) -> VmConfig {
    #[cfg(target_os = "linux")]
    let physical_block_size = {
        use std::os::unix::fs::MetadataExt;

        u32::try_from(fs::metadata(_vm.path()).unwrap().blksize()).unwrap()
    };
    #[cfg(windows)]
    let physical_block_size = 4096;
    #[cfg(not(any(target_os = "linux", windows)))]
    let physical_block_size = 512;
    let image = BlockGeometry {
        length: 16 << 20,
        logical_block_size: 512,
        physical_block_size,
    };
    VmConfig {
        processors: 2,
        boot: Some(Boot {
            memory_capacity_mib: None,
            command_line: "nvx_sandbox=1".to_owned(),
            capture: Some(CaptureConfig {
                disks: DiskCapture::Fresh {
                    image,
                    scratch: BlockGeometry {
                        length: 8 << 20,
                        ..image
                    },
                },
                processors: Capacity {
                    boot: 2,
                    capacity: 2,
                },
                memory: Capacity {
                    boot: 256,
                    capacity: 512,
                },
                invariants: Invariants::new().with("isolation.mode", "strict").unwrap(),
            }),
        }),
        ..cold_config()
    }
}

fn argument_values<'a>(arguments: &'a [OsString], option: &str) -> Vec<&'a OsStr> {
    arguments
        .windows(2)
        .filter(|pair| pair[0] == option)
        .map(|pair| pair[1].as_os_str())
        .collect()
}

#[test]
fn capture_creation_records_sparse_disks_devices_and_a_complete_serde_context() {
    for console in [ConsoleAttachment::Listener, ConsoleAttachment::Disconnected] {
        for nic in [
            None,
            Some(None),
            Some(Some(Ipv4Cidr::parse("10.42.0.2/24").unwrap())),
        ] {
            let (_directory, vm) = vm_dir();
            let mut runtime = cold_runtime();
            runtime.identity = RuntimeIdentity::new(
                ArtifactIdentity::new("capture-openvmm").unwrap(),
                ArtifactIdentity::new("capture-kernel").unwrap(),
                ArtifactIdentity::new("capture-initramfs").unwrap(),
                test_compatibility(),
            );
            runtime.cpu_profile = nic.map(|_| "amd.genoa.v1".to_owned());
            let mut policy = NetworkPolicy::new(Access::Allow);
            policy.ingress = Access::Allow;
            policy.host_loopback = HostLoopback::Allow;
            let config = VmConfig {
                console,
                network: nic.map(|guest_network| NetworkConfig {
                    guest_network,
                    policy,
                }),
                ..capture_config(&vm)
            };
            let prepared = create(&runtime, &config, None, &vm).unwrap();
            let context = prepared.capture_context().unwrap();
            assert_eq!(prepared.nic(), context.nic);
            assert_eq!(
                prepared
                    .guest_restore_record(TransactionId::from_bytes([0x61; 16]))
                    .unwrap_err()
                    .kind(),
                ErrorKind::Unsupported
            );
            assert_eq!(
                context.capture,
                *config.boot.as_ref().unwrap().capture.as_ref().unwrap()
            );
            assert_eq!(context.console, console);
            assert_eq!(context.runtime, runtime.identity);
            assert_eq!(context.hypervisor, runtime.hypervisor);
            assert_eq!(context.cpu_profile, runtime.cpu_profile);
            assert_eq!(
                context.nic,
                nic.map(|address| NicConfig {
                    guest_network: address
                        .unwrap_or_else(|| Ipv4Cidr::parse(DEFAULT_GUEST_NETWORK).unwrap()),
                })
            );
            let serialized = serde_json::to_string(context).unwrap();
            assert_eq!(
                serde_json::from_str::<CaptureContext>(&serialized).unwrap(),
                context.clone()
            );
            assert!(!serialized.contains("policy"));
            let DiskCapture::Fresh { image, scratch } = context.capture.disks;
            for (name, geometry) in [(CAPTURE_IMAGE, image), (CAPTURE_SCRATCH, scratch)] {
                let path = vm.path().join(name);
                let file = File::open(&path).unwrap();
                let metadata = file.metadata().unwrap();
                assert_eq!(metadata.len(), geometry.length);
                assert_eq!(platform::private_file_problem(&file).unwrap(), None);
                #[cfg(target_os = "linux")]
                {
                    use std::os::unix::fs::MetadataExt;

                    assert!(metadata.blocks() * 512 < metadata.len());
                    assert_eq!(metadata.mode() & 0o777, 0o600);
                    assert_eq!(u64::from(geometry.physical_block_size), metadata.blksize());
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;

                    assert_ne!(metadata.file_attributes() & 0x200, 0, "not a sparse file");
                }
            }
            let arguments = prepared.arguments();
            assert_eq!(
                argument_values(arguments, "--snapshot-tier"),
                [OsStr::new("platform")]
            );
            assert_eq!(
                argument_values(arguments, "--snapshot-destination"),
                [vm.path().join(OPENVMM).as_os_str()]
            );
            assert_eq!(
                argument_values(arguments, "--memory-capacity"),
                [OsStr::new("512M")]
            );
            assert_eq!(
                argument_values(arguments, "--microvm-sandbox-block"),
                [
                    OsStr::new(&format!(
                        "distro:file:{},ro",
                        vm.path().join(CAPTURE_IMAGE).display()
                    )),
                    OsStr::new(&format!(
                        "scratch:file:{}",
                        vm.path().join(CAPTURE_SCRATCH).display()
                    )),
                ]
            );
            assert_eq!(
                argument_values(arguments, "--cmdline"),
                [OsStr::new("nvx_sandbox=1")]
            );
            if let Some(nic) = context.nic {
                assert_eq!(
                    argument_values(arguments, "--net"),
                    [OsStr::new(&nic.guest_network.to_string())]
                );
                for option in ["--network-egress", "--network-ingress", "--host-loopback"] {
                    assert_eq!(argument_values(arguments, option), [OsStr::new("deny")]);
                }
            } else {
                assert!(argument_values(arguments, "--net").is_empty());
            }
            for option in [
                "--network-egress-allow",
                "--network-egress-deny",
                "--host-loopback-forward",
                "--proxy",
            ] {
                assert!(argument_values(arguments, option).is_empty(), "{option}");
            }
            assert_eq!(fs::read_dir(vm.path()).unwrap().count(), 3);
            for name in [OPENVMM, KEY_FILE, DESCRIPTION_FILE] {
                assert!(!vm.path().join(name).exists(), "{name}");
            }
        }
    }
}

#[test]
fn capture_creation_refuses_nonplatform_tokens_by_name_without_leaving_outputs() {
    for token in [
        "nokaslr",
        "loglevel=8",
        "nvx_entrypoint=/bin/sh",
        "nvx_arg=tenant",
        "nvx_share=/tenant",
        "nvx_workload_account=create",
        "nvx_layer=erofs",
        "nvx_hostname=tenant",
        "nvx_maps=1",
        "virtfs_tag=tenant",
        "custom=tenant",
        "earlycon=xe9",
        "console=hvc1",
        "virtio_mmio.device=0x1000@0xd0000000:1",
        "virtnet_ip=10.42.0.2",
        "virtnet_mask=255.255.255.0",
        "virtnet_gw=10.42.0.1",
        "virtnet_dns=10.42.0.1",
        "virtnet_ip6=fd00::2",
        "virtnet_gw6=fd00::1",
        "nvx_snapshot_tier=platform",
        "nvx_snapshot_tier=platform nvx_snapshot_tier=platform",
        "nr_cpus=2",
        "maxcpus=2",
        "nvx_control_tty=hvc2",
    ] {
        let (_directory, vm) = vm_dir();
        let mut config = capture_config(&vm);
        config.boot.as_mut().unwrap().command_line = token.to_owned();
        let error = create(&cold_runtime(), &config, None, &vm).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        assert!(
            error.to_string().contains(token.split('=').next().unwrap()),
            "{error}"
        );
        assert_eq!(fs::read_dir(vm.path()).unwrap().count(), 0);
    }
}

#[test]
fn capture_resources_not_the_vms_cold_resources_determine_the_launch() {
    for (boot, capacity) in [(1, 4), (2, 2)] {
        let (_directory, vm) = vm_dir();
        let mut config = capture_config(&vm);
        config.processors = 3;
        config.memory_mib = 17;
        let capture = config.boot.as_mut().unwrap().capture.as_mut().unwrap();
        capture.processors = Capacity { boot, capacity };
        capture.memory.boot = 128;
        let expected = capture.clone();
        let prepared = create(&cold_runtime(), &config, None, &vm).unwrap();
        assert_eq!(prepared.capture_context().unwrap().capture, expected);
        assert_eq!(
            argument_values(prepared.arguments(), "--processors"),
            [OsStr::new(&capacity.to_string())]
        );
        assert_eq!(
            argument_values(prepared.arguments(), "--memory"),
            [OsStr::new("128M")]
        );
        assert_eq!(
            argument_values(prepared.arguments(), "--memory-capacity"),
            [OsStr::new("512M")]
        );
        assert_eq!(
            argument_values(prepared.arguments(), "--cmdline"),
            [OsStr::new(if boot < capacity {
                "nvx_sandbox=1 maxcpus=1"
            } else {
                "nvx_sandbox=1"
            })]
        );
    }
}

#[test]
fn capture_creation_refuses_unaligned_boot_memory_before_creating_outputs() {
    let (_directory, vm) = vm_dir();
    let mut config = capture_config(&vm);
    config
        .boot
        .as_mut()
        .unwrap()
        .capture
        .as_mut()
        .unwrap()
        .memory
        .boot = 129;
    config.check().unwrap();
    let error = create(&cold_runtime(), &config, None, &vm).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
    assert!(
        error.to_string().contains("boot memory of 129 MiB"),
        "{error}"
    );
    assert_eq!(fs::read_dir(vm.path()).unwrap().count(), 0);
}

#[test]
fn capture_creation_refuses_layered_roots_and_exports_without_creating_outputs() {
    use crate::host::filesystem::{Bind, ExportChild, ExportPlan, Owner};

    let (_directory, vm) = vm_dir();
    let base = capture_config(&vm);
    let layered = VmConfig {
        root: RootConfig::Layered(LayeredRoot {
            image: vm.path().join("caller-image.erofs"),
            format: RootImage::erofs(None),
            gpt_first_partition_bound: None,
            upper: UpperConfig::Ram {
                max_memory_percent: 50,
            },
            overlay: OverlayMode::TrustedLowers,
        }),
        ..base.clone()
    };
    let exported = VmConfig {
        filesystem: Some(ExportPlan {
            children: vec![ExportChild {
                root: vm.path().to_owned(),
                file: false,
                writable: false,
                denied: Vec::new(),
                allowed: Vec::new(),
                write: Vec::new(),
            }],
            binds: vec![Bind {
                child: 0,
                source: String::new(),
                target: "/mnt/shared".to_owned(),
                read_only: true,
            }],
            owner: Owner::Vmm,
        }),
        ..base
    };
    for (config, refusal) in [(layered, "blockless"), (exported, "export")] {
        config.check().unwrap();
        let error = create(&cold_runtime(), &config, None, &vm).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        assert!(error.to_string().contains(refusal), "{error}");
        assert_eq!(fs::read_dir(vm.path()).unwrap().count(), 0);
    }
}

#[test]
fn capture_geometry_mismatches_roll_back_only_the_new_files() {
    for scratch_disk in [false, true] {
        for logical in [false, true] {
            let (_directory, vm) = vm_dir();
            let input = vm.path().join("caller-input");
            fs::write(&input, b"preserved").unwrap();
            let mut config = capture_config(&vm);
            let capture = config.boot.as_mut().unwrap().capture.as_mut().unwrap();
            let DiskCapture::Fresh { image, scratch } = &mut capture.disks;
            let geometry = if scratch_disk { scratch } else { image };
            if logical {
                geometry.logical_block_size = 1024;
                geometry.physical_block_size = geometry.physical_block_size.max(1024);
            } else {
                geometry.physical_block_size *= 2;
            }
            let error = create(&cold_runtime(), &config, None, &vm).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
            assert!(error.to_string().contains("actual geometry"), "{error}");
            assert!(
                error.to_string().contains(if scratch_disk {
                    CAPTURE_SCRATCH
                } else {
                    CAPTURE_IMAGE
                }),
                "{error}"
            );
            assert_eq!(fs::read_dir(vm.path()).unwrap().count(), 1);
            assert_eq!(fs::read(input).unwrap(), b"preserved");
        }
    }
}

#[test]
fn capture_creation_never_replaces_existing_disks_or_snapshot_destinations() {
    for name in [CAPTURE_IMAGE, CAPTURE_SCRATCH, OPENVMM] {
        let (_directory, vm) = vm_dir();
        let existing = vm.path().join(name);
        fs::write(&existing, b"preserved").unwrap();
        let error = create(&cold_runtime(), &capture_config(&vm), None, &vm).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{error}");
        assert!(error.to_string().contains(name), "{error}");
        assert_eq!(fs::read(&existing).unwrap(), b"preserved");
        assert_eq!(fs::read_dir(vm.path()).unwrap().count(), 1);
    }
}

#[test]
fn capture_creation_checks_the_complete_command_line_including_disk_and_destination_paths() {
    let (_directory, vm) = vm_dir();
    let config = capture_config(&vm);
    let runtime = cold_runtime();
    let prepared = create(&runtime, &config, None, &vm).unwrap();
    let length = process_command_line_units(runtime.openvmm.as_os_str(), prepared.arguments());
    drop(prepared);
    for name in [LOG, CAPTURE_IMAGE, CAPTURE_SCRATCH] {
        fs::remove_file(vm.path().join(name)).unwrap();
    }
    for extra in [0, 1] {
        let mut runtime = runtime.clone();
        runtime
            .openvmm
            .as_mut_os_string()
            .push("x".repeat(PROCESS_COMMAND_LINE_LIMIT - length + extra));
        let result = create(&runtime, &config, None, &vm);
        if extra == 0 {
            let prepared = result.unwrap();
            assert_eq!(
                process_command_line_units(runtime.openvmm.as_os_str(), prepared.arguments()),
                PROCESS_COMMAND_LINE_LIMIT
            );
            drop(prepared);
            for name in [LOG, CAPTURE_IMAGE, CAPTURE_SCRATCH] {
                fs::remove_file(vm.path().join(name)).unwrap();
            }
        } else {
            let error = result.unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
            assert!(
                error.to_string().contains("Windows process command line"),
                "{error}"
            );
        }
        assert_eq!(fs::read_dir(vm.path()).unwrap().count(), 0);
    }
}

fn layered_config() -> VmConfig {
    VmConfig {
        root: RootConfig::Layered(LayeredRoot {
            image: PathBuf::from("missing-image.erofs"),
            format: RootImage::erofs(Some(Uuid::from_bytes([0x11; 16]))),
            gpt_first_partition_bound: None,
            upper: UpperConfig::Ram {
                max_memory_percent: 33,
            },
            overlay: OverlayMode::UntrustedLowers,
        }),
        ..cold_config()
    }
}

#[test]
fn prepared_nic_is_the_effective_cold_device_not_the_callers_later_configuration() {
    let explicit = Ipv4Cidr::parse("192.0.2.9/24").unwrap();
    let default = Ipv4Cidr::parse(crate::host::config::DEFAULT_GUEST_NETWORK).unwrap();
    for (network, expected) in [
        (None, None),
        (
            Some(NetworkConfig {
                guest_network: Some(explicit),
                policy: NetworkPolicy::new(Access::Allow),
            }),
            Some(NicConfig {
                guest_network: explicit,
            }),
        ),
        (
            Some(NetworkConfig {
                guest_network: None,
                policy: NetworkPolicy::new(Access::Deny),
            }),
            Some(NicConfig {
                guest_network: default,
            }),
        ),
    ] {
        let (_directory, vm) = vm_dir();
        let mut config = cold_config();
        config.network = network;
        let prepared = create(&cold_runtime(), &config, None, &vm).unwrap();
        config.network = None;
        assert_eq!(prepared.nic(), expected);
    }
}

#[test]
fn prepared_record_keeps_checked_root_view_export_resources_and_transaction() {
    let erofs = layered_config();
    let gpt_root = LayeredRoot {
        image: PathBuf::from("missing-image.gpt"),
        format: RootImage::gpt(Some(Uuid::from_bytes([0x22; 16]))),
        gpt_first_partition_bound: Some(64 * 1024),
        upper: UpperConfig::Scratch {
            path: PathBuf::from("missing-scratch.raw"),
            journal: Journal::Forbidden,
        },
        overlay: OverlayMode::TrustedLowers,
    };
    let gpt = VmConfig {
        root: RootConfig::Layered(gpt_root.clone()),
        ..erofs.clone()
    };
    let unbounded = VmConfig {
        root: RootConfig::Layered(LayeredRoot {
            gpt_first_partition_bound: None,
            ..gpt_root
        }),
        ..gpt.clone()
    };
    for (mut config, image, upper, overlay) in [
        (
            erofs,
            Image::Erofs {
                uuid: Some(Uuid::from_bytes([0x11; 16])),
            },
            Upper::Tmpfs {
                max_memory_percent: 33,
            },
            OverlayMode::UntrustedLowers,
        ),
        (
            gpt,
            Image::Gpt {
                disk_guid: Some(Uuid::from_bytes([0x22; 16])),
                first_partition_bound: 64 * 1024,
            },
            Upper::Scratch {
                journal: Journal::Forbidden,
            },
            OverlayMode::TrustedLowers,
        ),
        (
            unbounded,
            Image::Gpt {
                disk_guid: Some(Uuid::from_bytes([0x22; 16])),
                first_partition_bound: u64::MAX,
            },
            Upper::Scratch {
                journal: Journal::Forbidden,
            },
            OverlayMode::TrustedLowers,
        ),
    ] {
        config.processors = 2;
        config.filesystem = Some(ExportPlan {
            children: vec![ExportChild {
                root: std::env::temp_dir().join("nvx-prepared-missing-export"),
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
        let expected_view = config.view.clone();
        let (_directory, vm) = vm_dir();
        let prepared = create(&cold_runtime(), &config, None, &vm).unwrap();
        config.processors = 8;
        config.view.hostname = b"changed".to_vec();
        config.filesystem = None;
        for bytes in [[0x35; 16], [0x71; 16]] {
            let tx = TransactionId::from_bytes(bytes);
            let record = prepared.guest_restore_record(tx).unwrap();
            assert_eq!(record.restore().unwrap().transaction, tx);
            let configuration = record.configuration();
            assert_eq!(configuration.image, image);
            assert_eq!(configuration.upper, upper);
            assert_eq!(configuration.overlay, overlay);
            assert_eq!(configuration.view, expected_view);
            assert_eq!(
                configuration.export,
                Some(Export {
                    kind: ExportKind::Aggregate,
                })
            );
            assert_eq!(configuration.mappings, 1);
            assert_eq!(configuration.network, None);
            assert_eq!(
                configuration.resources,
                ExpectedResources {
                    cpus: 2,
                    memory: 256 << 20,
                }
            );
            assert_eq!(
                GuestRestoreRecord::decode(&record.encode()).unwrap(),
                record
            );
        }
    }
}

#[test]
fn resolved_restore_record_uses_captured_presence_address_and_its_own_policy() {
    let captured = NicConfig {
        guest_network: Ipv4Cidr::parse("198.51.100.9/24").unwrap(),
    };
    let mut published = NetworkPolicy::new(Access::Allow);
    published.ingress = Access::Allow;
    for (policy, expected) in [
        (
            published,
            Network {
                dns: Some("198.51.100.1".parse().unwrap()),
                publication: Publication::Published,
                proxy: None,
            },
        ),
        (
            NetworkPolicy::new(Access::Deny),
            Network {
                dns: None,
                publication: Publication::Unpublished,
                proxy: None,
            },
        ),
        (
            NetworkPolicy::proxy_only(NetworkProxy::new(3128).with_scheme(ProxyScheme::Http)),
            Network {
                dns: None,
                publication: Publication::Unpublished,
                proxy: Some("http://198.51.100.1:3128".to_owned()),
            },
        ),
    ] {
        let mut config = layered_config();
        config.network = Some(NetworkConfig {
            guest_network: None,
            policy,
        });
        let (_directory, vm) = vm_dir();
        let mut prepared = create(&cold_runtime(), &config, None, &vm).unwrap();
        prepared.configuration.boot = None;
        prepared.nic = Some(captured);
        let tx = TransactionId::from_bytes([0x55; 16]);
        let record = prepared.guest_restore_record(tx).unwrap();
        assert_eq!(prepared.nic(), Some(captured));
        assert_eq!(record.restore().unwrap().transaction, tx);
        assert_eq!(record.configuration().network, Some(expected));
        prepared.nic = None;
        assert_eq!(prepared.nic(), None);
        assert_eq!(
            prepared
                .guest_restore_record(tx)
                .unwrap()
                .configuration()
                .network,
            None
        );
    }
}

#[test]
fn prepared_record_dns_uses_the_effective_nics_ipv6_gateway() {
    let explicit = Ipv4Cidr::parse("192.0.2.9/24").unwrap();
    let captured = NicConfig {
        guest_network: Ipv4Cidr::parse("198.51.100.9/24").unwrap(),
    };
    for (guest_network, restore_nic, expected) in [
        (None, None, "fd00::a00:1"),
        (Some(explicit), None, "fd00::c000:201"),
        (None, Some(captured), "fd00::c633:6401"),
    ] {
        let mut config = layered_config();
        config.network = Some(NetworkConfig {
            guest_network,
            policy: NetworkPolicy::new(Access::Deny)
                .with_allow(EgressRule::to(format!("{expected}/128")).on_port(Protocol::Udp, 53)),
        });
        let (_directory, vm) = vm_dir();
        let mut prepared = create(&cold_runtime(), &config, None, &vm).unwrap();
        if let Some(nic) = restore_nic {
            prepared.configuration.boot = None;
            prepared.nic = Some(nic);
        }
        let record = prepared
            .guest_restore_record(TransactionId::from_bytes([0x77; 16]))
            .unwrap();
        assert_eq!(
            record.configuration().network.as_ref().unwrap().dns,
            Some(expected.parse().unwrap())
        );
    }
}

#[test]
fn prepared_record_refuses_roots_or_memory_that_the_schema_cannot_represent() {
    for (config, message) in [
        (cold_config(), "a layered root"),
        (
            VmConfig {
                memory_mib: u64::MAX,
                ..layered_config()
            },
            "byte count",
        ),
    ] {
        let (_directory, vm) = vm_dir();
        let prepared = create(&cold_runtime(), &config, None, &vm).unwrap();
        let error = prepared
            .guest_restore_record(TransactionId::from_bytes([0x66; 16]))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert!(error.to_string().contains(message), "{error}");
    }
}

#[test]
fn cold_creation_is_inspectable_without_a_process_listener_or_capability_file() {
    for console in [ConsoleAttachment::Listener, ConsoleAttachment::Disconnected] {
        let (_directory, vm) = vm_dir();
        let runtime = cold_runtime();
        let config = VmConfig {
            console,
            ..cold_config()
        };
        let prepared = create(&runtime, &config, None, &vm).unwrap();
        assert!(prepared.capture_context().is_none());
        assert!(prepared.ready.is_none());
        let outputs = prepared.outputs();
        assert_eq!(outputs.log, vm.path().join("openvmm.log"));
        assert_eq!(
            outputs.console.is_some(),
            console == ConsoleAttachment::Listener
        );
        assert!(!outputs.outcome.exists());
        assert_eq!(prepared.capability().as_bytes().len(), CAPABILITY_LEN);
        assert_ne!(prepared.capability().as_bytes(), &[0; CAPABILITY_LEN]);
        assert_eq!(
            prepared.arguments(),
            config
                .openvmm_arguments(
                    &runtime,
                    &outputs.endpoint,
                    &outputs.outcome,
                    outputs.console.as_deref()
                )
                .unwrap()
        );
        assert_eq!(fs::read_dir(vm.path()).unwrap().count(), 1);
        drop(prepared);
        assert!(vm.path().join(LOG).exists());
    }
}

#[test]
fn a_missing_boot_and_boot_with_a_snapshot_are_refused_before_any_output_is_created() {
    let (_directory, vm) = vm_dir();
    let runtime = cold_runtime();
    let mut config = cold_config();
    config.boot = None;
    let fixture = crate::host::snapshot::tests::SnapshotFixture::new();
    let snapshot = Snapshot::open(&fixture.dir).unwrap();
    for (from, config, expected) in [
        (None, &config, "Boot"),
        (Some(&snapshot), &cold_config(), "Snapshot"),
    ] {
        let error = create(&runtime, config, from, &vm).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        assert!(error.to_string().contains(expected), "{error}");
        assert_eq!(fs::read_dir(vm.path()).unwrap().count(), 0);
    }
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn snapshot_creation_checks_eligibility_and_real_host_time_before_creating_outputs() {
    use crate::host::snapshot::tests::RestoreFixture;

    for refusal in [
        "processors",
        "memory",
        "runtime",
        "backend",
        "profile",
        "NIC",
        "address",
        "console",
        "early",
        "expired",
    ] {
        let mut fixture = RestoreFixture::current(true);
        let mut kind = ErrorKind::Unsupported;
        match refusal {
            "processors" => fixture.config.processors = 8,
            "memory" => fixture.config.memory_mib = 129,
            "runtime" => {
                fixture.runtime.identity.kernel = ArtifactIdentity::new("other-kernel").unwrap()
            }
            "backend" => fixture.runtime.hypervisor = Hypervisor::Mshv,
            "profile" => fixture.runtime.cpu_profile = Some("different".to_owned()),
            "NIC" => fixture.config.network = None,
            "address" => {
                fixture.config.network.as_mut().unwrap().guest_network =
                    Some(Ipv4Cidr::parse("10.43.0.2/24").unwrap())
            }
            "console" => fixture.config.console = ConsoleAttachment::Listener,
            "early" | "expired" => {
                let mut time = HostTime::now().unwrap();
                time.host_id[0] ^= 1;
                let outside = crate::host::snapshot::MAX_DOWNTIME_NS + 1_000_000_000;
                time.utc_ns = if refusal == "early" {
                    time.utc_ns + outside
                } else {
                    time.utc_ns - outside
                };
                fixture.retime(time);
                kind = ErrorKind::Unavailable;
            }
            _ => unreachable!(),
        }
        let snapshot = fixture.opened();
        let (_directory, vm) = vm_dir();
        let error = create(&fixture.runtime, &fixture.config, Some(&snapshot), &vm).unwrap_err();
        assert_eq!(error.kind(), kind, "{refusal}: {error}");
        assert_eq!(fs::read_dir(vm.path()).unwrap().count(), 0, "{refusal}");
    }
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn snapshot_creation_owns_the_fixed_view_and_uses_captured_nic_with_requested_policy() {
    use crate::host::snapshot::tests::RestoreFixture;

    let mut published = NetworkPolicy::new(Access::Allow);
    published.ingress = Access::Allow;
    for (policy, expected) in [
        (
            published,
            Network {
                dns: Some("10.42.0.1".parse().unwrap()),
                publication: Publication::Published,
                proxy: None,
            },
        ),
        (
            NetworkPolicy::new(Access::Deny),
            Network {
                dns: None,
                publication: Publication::Unpublished,
                proxy: None,
            },
        ),
        (
            NetworkPolicy::proxy_only(NetworkProxy::new(3128).with_scheme(ProxyScheme::Http)),
            Network {
                dns: None,
                publication: Publication::Unpublished,
                proxy: Some("http://10.42.0.1:3128".to_owned()),
            },
        ),
    ] {
        let mut fixture = RestoreFixture::current(true);
        fixture.config.network.as_mut().unwrap().policy = policy;
        let snapshot = fixture.opened();
        let (_directory, vm) = vm_dir();
        let input = vm.path().join("caller-input");
        fs::write(&input, b"preserved").unwrap();
        let prepared = create(&fixture.runtime, &fixture.config, Some(&snapshot), &vm).unwrap();
        assert_eq!(prepared.nic(), snapshot.nic());
        assert!(prepared.capture_context().is_none());
        assert!(prepared.ready.is_some());
        let record = prepared
            .guest_restore_record(TransactionId::from_bytes([0x31; 16]))
            .unwrap();
        assert_eq!(record.configuration().network, Some(expected));
        assert!(prepared.inherit.is_empty());
        for name in crate::host::snapshot::SNAPSHOT_ARTIFACTS {
            assert_eq!(
                platform::file_identity(&snapshot.dir.join(OPENVMM).join(name)).unwrap(),
                platform::file_identity(&vm.path().join(OPENVMM).join(name)).unwrap()
            );
        }
        drop(prepared);
        assert_eq!(fs::read_dir(vm.path().join(OPENVMM)).unwrap().count(), 3);
        assert_eq!(fs::read(&input).unwrap(), b"preserved");
    }
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn snapshot_creation_never_replaces_existing_outputs_views_or_listeners() {
    use crate::host::snapshot::tests::RestoreFixture;

    let mut names = vec![
        crate::host::vm_directory::LOG,
        crate::host::vm_directory::OUTCOME,
        OPENVMM,
    ];
    if !cfg!(windows) {
        names.extend([CONTROL, RESTORE_READY]);
    }
    for name in names {
        let fixture = RestoreFixture::current(false);
        let snapshot = fixture.opened();
        let (_directory, vm) = vm_dir();
        let path = vm.path().join(name);
        let preserved = if name == OPENVMM {
            fs::create_dir(&path).unwrap();
            path.join("caller-file")
        } else {
            path
        };
        fs::write(&preserved, b"existing").unwrap();
        let error = create(&fixture.runtime, &fixture.config, Some(&snapshot), &vm).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{name}: {error}");
        assert_eq!(fs::read(&preserved).unwrap(), b"existing");
        assert_eq!(fs::read_dir(vm.path()).unwrap().count(), 1);
    }
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn snapshot_creation_rolls_back_partial_links_and_a_render_failure_after_binding() {
    use crate::host::snapshot::tests::RestoreFixture;
    use std::error::Error as _;

    for partial_link in [false, true] {
        let mut fixture = RestoreFixture::current(false);
        let snapshot = fixture.opened();
        if partial_link {
            fs::remove_file(snapshot.dir.join(OPENVMM).join("memory.bin")).unwrap();
        } else {
            fixture.runtime.openvmm = PathBuf::from("x".repeat(PROCESS_COMMAND_LINE_LIMIT));
        }
        let (_directory, vm) = vm_dir();
        let input = vm.path().join("caller-input");
        fs::write(&input, b"preserved").unwrap();
        let error = create(&fixture.runtime, &fixture.config, Some(&snapshot), &vm).unwrap_err();
        if partial_link {
            assert_eq!(error.kind(), ErrorKind::Io, "{error}");
            assert_eq!(
                error
                    .source()
                    .unwrap()
                    .downcast_ref::<io::Error>()
                    .unwrap()
                    .kind(),
                io::ErrorKind::NotFound
            );
        } else {
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
            assert!(
                error.to_string().contains("Windows process command line"),
                "{error}"
            );
        }
        assert_eq!(fs::read(&input).unwrap(), b"preserved");
        assert_eq!(fs::read_dir(vm.path()).unwrap().count(), 1);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn the_same_prebound_listener_keeps_its_queue_through_both_launch_paths_without_inheritance() {
    use crate::host::snapshot::tests::RestoreFixture;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    for external in [false, true] {
        let fixture = RestoreFixture::current(false);
        let snapshot = fixture.opened();
        let (_directory, vm) = vm_dir();
        let mut prepared = create(&fixture.runtime, &fixture.config, Some(&snapshot), &vm).unwrap();
        let endpoint = vm.path().join(RESTORE_READY);
        let original = platform::file_identity(&endpoint).unwrap();
        let queued = UnixStream::connect(&endpoint).unwrap();
        let mut running = None;
        let listener = if external {
            let command = prepared.into_command();
            assert!(command.inherit.is_empty());
            command.ready.unwrap().into_std().unwrap()
        } else {
            (prepared.program, prepared.arguments) = stand_in_command();
            running = Some(RunningVm(spawn(prepared).unwrap()));
            running
                .as_mut()
                .unwrap()
                .0
                .ready
                .take()
                .unwrap()
                .into_std()
                .unwrap()
        };
        assert_eq!(platform::file_identity(&endpoint).unwrap(), original);
        let (accepted, _) = listener
            .accept()
            .expect("the connection queued before handoff remains");
        assert_eq!(
            listener.local_addr().unwrap().as_pathname(),
            Some(endpoint.as_path())
        );
        if let Some(running) = &running {
            let socket = fs::read_link(format!("/proc/self/fd/{}", listener.as_raw_fd())).unwrap();
            let descriptors = format!("/proc/{}/fd", running.0.process.reference().pid);
            for entry in fs::read_dir(descriptors).unwrap() {
                match fs::read_link(entry.unwrap().path()) {
                    Ok(inherited) => assert_ne!(
                        inherited, socket,
                        "the caller's ready listener reached the child"
                    ),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => panic!("cannot inspect the owned child descriptor: {error}"),
                }
            }
        }
        drop((accepted, queued, running));
    }
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn snapshot_creation_keeps_captured_console_attachment_and_zero_boot_processor_rules() {
    use crate::host::snapshot::tests::RestoreFixture;

    for listener in [false, true] {
        let mut fixture = RestoreFixture::current(false);
        fixture.all_processors_online();
        if listener {
            fixture.console_listener();
        }
        let snapshot = fixture.opened();
        let (_directory, vm) = vm_dir();
        let prepared = create(&fixture.runtime, &fixture.config, Some(&snapshot), &vm).unwrap();
        assert!(
            !prepared
                .arguments()
                .iter()
                .any(|arg| arg == "--restore-processors")
        );
        let console = prepared
            .arguments()
            .windows(2)
            .find(|pair| pair[0] == "--virtio-console")
            .unwrap();
        assert_eq!(
            console[1],
            if listener {
                OsString::from(format!(
                    "listen={}",
                    prepared.outputs().console.as_ref().unwrap()
                ))
            } else {
                OsString::from("none")
            }
        );
    }
}

#[cfg(any(target_os = "linux", windows))]
#[test]
#[ignore = "requires an explicitly approved second filesystem in NVX_VM_TEST_OTHER_FILESYSTEM"]
fn snapshot_creation_refuses_a_true_cross_filesystem_view_without_copying() {
    use crate::host::snapshot::tests::RestoreFixture;
    use std::error::Error as _;

    let other = std::env::var_os("NVX_VM_TEST_OTHER_FILESYSTEM")
        .expect("an explicitly approved second filesystem is required, not a skipped assertion");
    let fixture = RestoreFixture::current(false);
    let snapshot = fixture.opened();
    let directory = tempfile::tempdir_in(other).unwrap();
    let path = directory.path().join("vm");
    create_private_dir(&path).unwrap();
    let vm = VmDir::open(&path).unwrap();
    let input = vm.path().join("caller-input");
    fs::write(&input, b"preserved").unwrap();
    assert_ne!(
        platform::file_identity(&input).unwrap().0,
        platform::file_identity(&snapshot.dir.join(OPENVMM).join("memory.bin"))
            .unwrap()
            .0,
        "the test must actually cross filesystems"
    );
    let error = create(&fixture.runtime, &fixture.config, Some(&snapshot), &vm).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    let source = error.source().unwrap().downcast_ref::<io::Error>().unwrap();
    eprintln!(
        "strict cross-filesystem refusal: native={:?}; source_fs={}; destination_fs={}; {error}",
        source.raw_os_error(),
        platform::file_identity(&snapshot.dir.join(OPENVMM).join("memory.bin"))
            .unwrap()
            .0,
        platform::file_identity(&input).unwrap().0,
    );
    #[cfg(target_os = "linux")]
    assert_eq!(source.raw_os_error(), Some(nix::libc::EXDEV));
    #[cfg(windows)]
    assert_eq!(
        source.raw_os_error(),
        Some(windows_sys::Win32::Foundation::ERROR_NOT_SAME_DEVICE as i32)
    );
    assert_eq!(fs::read(&input).unwrap(), b"preserved");
    assert_eq!(fs::read_dir(vm.path()).unwrap().count(), 1);
}

#[test]
fn restore_arguments_use_snapshot_capacity_current_policy_and_no_cold_inputs() {
    use crate::host::snapshot::tests::RestoreFixture;

    for upper_in_ram in [false, true] {
        for policy in [
            NetworkPolicy::new(Access::Allow),
            NetworkPolicy::new(Access::Deny),
            NetworkPolicy::proxy_only(NetworkProxy::new(3128)),
        ] {
            let mut fixture = RestoreFixture::new(true);
            fixture.config.network.as_mut().unwrap().policy = policy.clone();
            fixture.config.processors = 4;
            fixture.config.memory_mib = 768;
            if upper_in_ram {
                let RootConfig::Layered(root) = &mut fixture.config.root else {
                    unreachable!()
                };
                root.upper = UpperConfig::Ram {
                    max_memory_percent: 50,
                };
            }
            let snapshot = fixture.opened();
            let (_directory, vm) = vm_dir();
            let outputs = vm.outputs(false).unwrap();
            let ready = vm.restore_ready_endpoint().unwrap();
            let arguments = restore_arguments(
                &fixture.runtime,
                &fixture.config,
                &snapshot,
                &vm,
                &outputs,
                &ready,
            )
            .unwrap();
            let value = |name| {
                arguments
                    .windows(2)
                    .find(|pair| pair[0] == name)
                    .map(|pair| pair[1].clone())
            };
            assert_eq!(value("--processors"), Some("4".into()));
            assert_eq!(value("--restore-processors"), Some("4".into()));
            assert_eq!(value("--restore-memory"), Some("768M".into()));
            assert_eq!(
                value("--restore-snapshot"),
                Some(vm.path().join(OPENVMM).into_os_string())
            );
            assert_eq!(value("--restore-ready-path"), Some(ready.into()));
            assert_eq!(value("--virtio-console"), Some("none".into()));
            assert_eq!(
                value("--cpu-profile"),
                Some(snapshot.key().cpu_profile().into())
            );
            assert_eq!(
                arguments
                    .iter()
                    .filter(|arg| *arg == "--restore-network-policy")
                    .count(),
                1
            );
            assert_eq!(
                arguments
                    .iter()
                    .any(|arg| arg == "--restore-absent-scratch"),
                upper_in_ram
            );
            assert_eq!(
                arguments
                    .iter()
                    .filter(|arg| *arg == "--microvm-sandbox-block")
                    .count(),
                if upper_in_ram { 1 } else { 2 }
            );
            for forbidden in [
                "--net",
                "--kernel",
                "--initrd",
                "--cmdline",
                "--memory",
                "--snapshot-destination",
            ] {
                assert!(!arguments.iter().any(|arg| arg == forbidden), "{forbidden}");
            }
            let proxy = policy.proxy.is_some();
            assert_eq!(
                value("--network-proxy"),
                proxy.then(|| "10.42.0.1:3128".into())
            );
            assert_eq!(fs::read_dir(vm.path()).unwrap().count(), 0);
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn cold_creation_refuses_listener_delimiters_before_creating_outputs() {
    for delimiter in [',', '='] {
        for console in [ConsoleAttachment::Listener, ConsoleAttachment::Disconnected] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join(format!("vm{delimiter}option"));
            create_private_dir(&path).unwrap();
            let vm = VmDir::open(&path).unwrap();
            let input = path.join("caller-input");
            fs::write(&input, b"borrowed input").unwrap();
            let config = VmConfig {
                console,
                ..cold_config()
            };
            let error = create(&cold_runtime(), &config, None, &vm).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
            assert_eq!(fs::read(&input).unwrap(), b"borrowed input");
            assert_eq!(fs::read_dir(&path).unwrap().count(), 1);
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn cold_creation_checks_socket_path_byte_limits_before_creating_outputs() {
    use std::os::unix::net::UnixListener;

    for socket_bytes in [107usize, 108, 109] {
        for console in [ConsoleAttachment::Listener, ConsoleAttachment::Disconnected] {
            let directory = tempfile::tempdir().unwrap();
            let name_bytes = socket_bytes
                .checked_sub(directory.path().as_os_str().len() + CONTROL.len() + 2)
                .expect("a short temporary directory is needed for the socket boundary");
            let prefix = "unicode space ";
            let remaining = name_bytes.checked_sub(prefix.len()).unwrap();
            let mut name = format!("{prefix}{}", "\u{e9}".repeat(remaining / 2));
            name.push_str(&"x".repeat(remaining % 2));
            let path = directory.path().join(name);
            create_private_dir(&path).unwrap();
            let vm = VmDir::open(&path).unwrap();
            let outputs = vm.outputs(console == ConsoleAttachment::Listener).unwrap();
            assert_eq!(outputs.endpoint.len(), socket_bytes);
            assert!(outputs.endpoint.encode_utf16().count() < socket_bytes);
            let input = path.join("caller-input");
            fs::write(&input, b"borrowed input").unwrap();
            let config = VmConfig {
                console,
                ..cold_config()
            };
            let result = create(&cold_runtime(), &config, None, &vm);
            if socket_bytes == 107 {
                let prepared = result.unwrap();
                for endpoint in std::iter::once(&prepared.outputs().endpoint)
                    .chain(prepared.outputs().console.as_ref())
                {
                    let listener = UnixListener::bind(endpoint).unwrap();
                    assert_eq!(
                        listener.local_addr().unwrap().as_pathname(),
                        Some(Path::new(endpoint))
                    );
                }
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
                assert_eq!(fs::read_dir(&path).unwrap().count(), 1);
            }
            assert_eq!(fs::read(&input).unwrap(), b"borrowed input");
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn cold_creation_checks_canonical_socket_byte_limits_through_ancestor_symlinks() {
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;

    for socket_bytes in [107usize, 108, 109] {
        for console in [ConsoleAttachment::Listener, ConsoleAttachment::Disconnected] {
            let directory = tempfile::tempdir().unwrap();
            let root = fs::canonicalize(directory.path()).unwrap();
            let name_bytes = socket_bytes
                .checked_sub(root.as_os_str().len() + CONTROL.len() + 5)
                .expect("a short temporary directory is needed for the socket boundary");
            let prefix = "unicode space,= ";
            let remaining = name_bytes.checked_sub(prefix.len()).unwrap();
            let mut name = format!("{prefix}{}", "\u{e9}".repeat(remaining / 2));
            name.push_str(&"x".repeat(remaining % 2));
            let target = root.join(name);
            fs::create_dir(&target).unwrap();
            let path = target.join("vm");
            create_private_dir(&path).unwrap();
            let alias = root.join("alias");
            symlink(&target, &alias).unwrap();
            let vm = VmDir::open(&alias.join("vm")).unwrap();
            let outputs = vm.outputs(console == ConsoleAttachment::Listener).unwrap();
            assert!(outputs.endpoint.len() < socket_bytes);
            assert_eq!(path.join(CONTROL).as_os_str().len(), socket_bytes);
            let input = path.join("caller-input");
            fs::write(&input, b"borrowed input").unwrap();
            let config = VmConfig {
                console,
                ..cold_config()
            };
            let result = create(&cold_runtime(), &config, None, &vm);
            if socket_bytes == 107 {
                let prepared = result.unwrap();
                assert_eq!(prepared.outputs(), &outputs);
                for name in std::iter::once(CONTROL)
                    .chain(prepared.outputs().console.as_ref().map(|_| CONSOLE))
                {
                    let resolved = path.join(name);
                    let listener = UnixListener::bind(&resolved).unwrap();
                    assert_eq!(
                        listener.local_addr().unwrap().as_pathname(),
                        Some(resolved.as_path())
                    );
                }
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
                assert_eq!(fs::read_dir(&path).unwrap().count(), 1);
            }
            assert_eq!(fs::read(&input).unwrap(), b"borrowed input");
            assert!(
                fs::symlink_metadata(&alias)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn cold_creation_refuses_non_unicode_canonical_sockets_through_ancestor_symlinks() {
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;

    for console in [ConsoleAttachment::Listener, ConsoleAttachment::Disconnected] {
        let directory = tempfile::tempdir().unwrap();
        let target = directory
            .path()
            .join(OsString::from_vec(b"parent-\xff".to_vec()));
        fs::create_dir(&target).unwrap();
        let path = target.join("vm");
        create_private_dir(&path).unwrap();
        let alias = directory.path().join("alias");
        symlink(&target, &alias).unwrap();
        let vm = VmDir::open(&alias.join("vm")).unwrap();
        assert!(vm.path().to_str().is_some());
        assert!(fs::canonicalize(vm.path()).unwrap().to_str().is_none());
        let input = path.join("caller-input");
        fs::write(&input, b"borrowed input").unwrap();
        let config = VmConfig {
            console,
            ..cold_config()
        };
        let error = create(&cold_runtime(), &config, None, &vm).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        assert!(error.to_string().contains("Unicode"), "{error}");
        assert_eq!(fs::read(&input).unwrap(), b"borrowed input");
        assert_eq!(fs::read_dir(&path).unwrap().count(), 1);
        assert!(
            fs::symlink_metadata(&alias)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}

#[test]
fn creation_rolls_back_its_log_and_never_removes_preexisting_outputs_or_inputs() {
    let mut names = vec![LOG, OUTCOME];
    if !cfg!(windows) {
        names.extend([CONTROL, CONSOLE]);
    }
    for name in names {
        let (_directory, vm) = vm_dir();
        let existing = vm.path().join(name);
        let input = vm.path().join("caller-input");
        fs::write(&existing, b"preexisting output").unwrap();
        fs::write(&input, b"borrowed input").unwrap();
        let error = create(&cold_runtime(), &cold_config(), None, &vm).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{error}");
        assert_eq!(fs::read(&existing).unwrap(), b"preexisting output");
        assert_eq!(fs::read(&input).unwrap(), b"borrowed input");
        assert_eq!(fs::read_dir(vm.path()).unwrap().count(), 2);
    }
}

#[test]
fn the_log_and_inherited_resources_stay_open_without_reopening_their_paths() {
    use std::io::Write;
    let (_directory, vm) = vm_dir();
    let mut prepared = create(&cold_runtime(), &cold_config(), None, &vm).unwrap();
    let log = prepared.outputs().log.clone();
    let moved_log = vm.path().join("original-log");
    fs::rename(&log, &moved_log).unwrap();
    fs::write(&log, b"replacement log").unwrap();
    prepared
        .log()
        .try_clone()
        .unwrap()
        .write_all(b"original log")
        .unwrap();
    assert_eq!(fs::read(&moved_log).unwrap(), b"original log");
    assert_eq!(fs::read(&log).unwrap(), b"replacement log");
    let input = vm.path().join("caller-input");
    let moved_input = vm.path().join("original-input");
    let file = File::create(&input).unwrap();
    prepared.inherit(file);
    fs::rename(&input, &moved_input).unwrap();
    fs::write(&input, b"replacement input").unwrap();
    inherited_file(prepared.inherit.pop().unwrap())
        .write_all(b"original input")
        .unwrap();
    assert_eq!(fs::read(&moved_input).unwrap(), b"original input");
    assert_eq!(fs::read(&input).unwrap(), b"replacement input");
}

fn inherited_file(resource: InheritedHandle) -> File {
    #[cfg(any(unix, windows))]
    return File::from(Owned::from(resource));
    #[cfg(not(any(unix, windows)))]
    resource.handle
}

#[test]
fn a_command_moves_the_checked_launch_and_the_same_opened_resources() {
    use std::io::Write;

    let (_directory, vm) = vm_dir();
    let mut runtime = cold_runtime();
    runtime.windows_breakaway = true;
    let mut prepared = create(&runtime, &cold_config(), None, &vm).unwrap();
    let program = absolute(&runtime.openvmm).unwrap();
    let arguments = prepared.arguments().as_ptr();
    let capability = *prepared.capability().as_bytes();
    let outputs = prepared.outputs().clone();
    let original_log = vm.path().join("original-log");
    fs::rename(&outputs.log, &original_log).unwrap();
    fs::write(&outputs.log, b"replacement log").unwrap();
    let input = vm.path().join("input");
    let original_input = vm.path().join("original-input");
    prepared.inherit(File::create(&input).unwrap());
    let resources = prepared.inherit.as_ptr();
    fs::rename(&input, &original_input).unwrap();
    fs::write(&input, b"replacement input").unwrap();
    #[cfg(unix)]
    let log_handle = std::os::fd::AsRawFd::as_raw_fd(prepared.log());
    #[cfg(windows)]
    let log_handle = std::os::windows::io::AsRawHandle::as_raw_handle(prepared.log());
    runtime.openvmm = "different-program".into();
    runtime.windows_breakaway = false;

    let mut command = prepared.into_command();
    assert_eq!(command.program, program);
    assert_eq!(command.working_dir, vmm_working_dir(vm.path()).unwrap());
    assert!(command.breakaway_from_job);
    assert_eq!(command.arguments.as_ptr(), arguments);
    assert_eq!(command.inherit.as_ptr(), resources);
    assert_eq!(command.capability.as_bytes(), &capability);
    assert_eq!(command.outputs, outputs);
    #[cfg(unix)]
    assert_eq!(std::os::fd::AsRawFd::as_raw_fd(&command.log), log_handle);
    #[cfg(windows)]
    assert_eq!(
        std::os::windows::io::AsRawHandle::as_raw_handle(&command.log),
        log_handle
    );
    command.log.write_all(b"original log").unwrap();
    inherited_file(command.inherit.pop().unwrap())
        .write_all(b"original input")
        .unwrap();
    assert_eq!(fs::read(original_log).unwrap(), b"original log");
    assert_eq!(fs::read(original_input).unwrap(), b"original input");
    assert_eq!(fs::read(&outputs.log).unwrap(), b"replacement log");
    assert_eq!(fs::read(input).unwrap(), b"replacement input");
    drop(command);
    assert!(outputs.log.exists());
    assert!(!outputs.outcome.exists());
}

#[cfg(unix)]
#[test]
fn an_owned_descriptor_moves_into_and_out_of_an_inherited_handle() {
    use std::io::Write;
    use std::os::fd::{AsFd, AsRawFd, OwnedFd};

    let file = tempfile::tempfile().unwrap();
    let number = file.as_raw_fd();
    let descriptor: OwnedFd = file.into();
    let resource: InheritedHandle = descriptor.into();
    assert_eq!(resource.as_fd().as_raw_fd(), number);
    let descriptor: OwnedFd = resource.into();
    assert_eq!(descriptor.as_raw_fd(), number);
    File::from(descriptor).write_all(b"still open").unwrap();
}

#[cfg(windows)]
#[test]
fn an_owned_windows_handle_moves_into_and_out_of_an_inherited_handle() {
    use std::io::Write;
    use std::os::windows::io::{AsHandle, AsRawHandle, OwnedHandle};

    let file = tempfile::tempfile().unwrap();
    let number = file.as_raw_handle();
    let handle: OwnedHandle = file.into();
    let resource: InheritedHandle = handle.into();
    assert_eq!(resource.as_handle().as_raw_handle(), number);
    let handle: OwnedHandle = resource.into();
    assert_eq!(handle.as_raw_handle(), number);
    File::from(handle).write_all(b"still open").unwrap();
}

#[test]
fn creation_checks_the_complete_actual_command_line_at_the_windows_boundary() {
    let (_directory, vm) = vm_dir();
    let config = cold_config();
    config.check_command_line().unwrap();
    let mut runtime = cold_runtime();
    let outputs = vm.outputs(true).unwrap();
    let arguments = config
        .openvmm_arguments(
            &runtime,
            &outputs.endpoint,
            &outputs.outcome,
            outputs.console.as_deref(),
        )
        .unwrap();
    let length = process_command_line_units(runtime.openvmm.as_os_str(), &arguments);
    runtime
        .openvmm
        .as_mut_os_string()
        .push("x".repeat(PROCESS_COMMAND_LINE_LIMIT - length));
    let prepared = create(&runtime, &config, None, &vm).unwrap();
    assert_eq!(
        process_command_line_units(runtime.openvmm.as_os_str(), prepared.arguments()),
        PROCESS_COMMAND_LINE_LIMIT
    );
    let log = prepared.outputs().log.clone();
    drop(prepared);
    fs::remove_file(&log).unwrap();
    let input = vm.path().join("caller-input");
    fs::write(&input, b"borrowed input").unwrap();
    runtime.openvmm.as_mut_os_string().push("x");
    let error = create(&runtime, &config, None, &vm).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
    assert!(error.to_string().contains("32768"), "{error}");
    assert!(!log.exists());
    assert_eq!(fs::read(&input).unwrap(), b"borrowed input");
    assert_eq!(fs::read_dir(vm.path()).unwrap().count(), 1);
}

#[cfg(any(target_os = "linux", windows))]
fn stand_in_command() -> (PathBuf, Vec<OsString>) {
    #[cfg(target_os = "linux")]
    let command = (PathBuf::from("/bin/sleep"), vec![OsString::from("60")]);
    #[cfg(windows)]
    let command = (
        PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32\\PING.EXE"),
        ["-n", "60", "127.0.0.1"]
            .into_iter()
            .map(OsString::from)
            .collect(),
    );
    command
}

#[cfg(any(target_os = "linux", windows))]
fn stand_in(dir: &VmDir) -> Prepared {
    let (program, arguments) = stand_in_command();
    let runtime = Runtime {
        openvmm: program,
        ..cold_runtime()
    };
    let mut prepared = create(&runtime, &cold_config(), None, dir).unwrap();
    prepared.arguments = arguments;
    prepared
}

#[cfg(any(target_os = "linux", windows))]
struct RunningVm(Launched);

#[test]
fn vmms_start_in_the_system_directory_on_windows_and_in_their_vm_directory_elsewhere() {
    let (_directory, vm) = vm_dir();
    #[cfg(not(windows))]
    assert_eq!(vmm_working_dir(vm.path()).unwrap(), vm.path());
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        let system = Path::new(&std::env::var_os("SystemRoot").unwrap()).join("System32");
        assert!(system.is_dir());
        assert!(system.as_os_str().encode_wide().count() <= 258);
        let working_dir = vmm_working_dir(vm.path()).unwrap();
        assert!(
            working_dir
                .as_os_str()
                .eq_ignore_ascii_case(system.as_os_str()),
            "{}",
            working_dir.display()
        );
    }
}

/// A VM directory whose path exceeds the 258 UTF-16 code units of the working directory in which
/// Windows starts a process still creates and spawns its VMM, which starts in the system
/// directory.
#[cfg(windows)]
#[test]
fn a_vm_directory_beyond_the_windows_working_directory_limit_spawns_its_vmm() {
    use std::os::windows::ffi::OsStrExt;

    let root = tempfile::tempdir().unwrap();
    let mut parent = root.path().to_path_buf();
    while parent.as_os_str().encode_wide().count() < 320 {
        parent.push("long-vm-directory-component");
    }
    fs::create_dir_all(&parent).unwrap();
    let path = parent.join("vm");
    create_private_dir(&path).unwrap();
    let vm = VmDir::open(&path).unwrap();
    assert!(vm.path().as_os_str().encode_wide().count() > 258);
    let prepared = stand_in(&vm);
    assert_eq!(prepared.working_dir, vmm_working_dir(vm.path()).unwrap());
    let launched = RunningVm(spawn(prepared).unwrap());
    assert!(!launched.0.process.has_exited().unwrap());
}

#[cfg(any(target_os = "linux", windows))]
impl Drop for RunningVm {
    fn drop(&mut self) {
        let _ = self.0.process.kill();
        let _ = self
            .0
            .process
            .wait(Instant::now() + Duration::from_secs(10));
    }
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn spawn_resolves_relative_runtime_and_block_inputs_outside_the_vm_directory() {
    let cwd = std::env::current_dir().unwrap();
    let inputs = tempfile::tempdir_in(&cwd).unwrap();
    let relative = inputs.path().strip_prefix(&cwd).unwrap();
    let (program, arguments) = stand_in_command();
    let executable = format!("stand-in{}", std::env::consts::EXE_SUFFIX);
    fs::copy(program, inputs.path().join(&executable)).unwrap();
    for name in ["kernel", "initramfs", "image.erofs", "scratch.raw"] {
        fs::write(inputs.path().join(name), name.as_bytes()).unwrap();
    }
    let root = |image, scratch| {
        RootConfig::Layered(LayeredRoot {
            image,
            format: RootImage::erofs(None),
            gpt_first_partition_bound: None,
            upper: UpperConfig::Scratch {
                path: scratch,
                journal: Journal::Forbidden,
            },
            overlay: OverlayMode::TrustedLowers,
        })
    };
    let runtime = Runtime {
        openvmm: relative.join(&executable),
        kernel: relative.join("kernel"),
        initramfs: relative.join("initramfs"),
        ..cold_runtime()
    };
    let config = VmConfig {
        root: root(relative.join("image.erofs"), relative.join("scratch.raw")),
        ..cold_config()
    };
    let (_directory, vm) = vm_dir();
    let mut prepared = create(&runtime, &config, None, &vm).unwrap();
    let resolved_runtime = Runtime {
        openvmm: inputs.path().join(&executable),
        kernel: inputs.path().join("kernel"),
        initramfs: inputs.path().join("initramfs"),
        ..runtime
    };
    let resolved_config = VmConfig {
        root: root(
            inputs.path().join("image.erofs"),
            inputs.path().join("scratch.raw"),
        ),
        ..config
    };
    assert_eq!(prepared.program, resolved_runtime.openvmm);
    assert_eq!(
        prepared.arguments(),
        resolved_config
            .openvmm_arguments(
                &resolved_runtime,
                &prepared.outputs().endpoint,
                &prepared.outputs().outcome,
                prepared.outputs().console.as_deref(),
            )
            .unwrap()
    );
    prepared.arguments = arguments;
    let launched = RunningVm(spawn(prepared).unwrap());
    assert!(!launched.0.process.has_exited().unwrap());
}

#[test]
fn resolving_inputs_preserves_required_path_errors_and_log_rollback() {
    for kernel in [PathBuf::new(), PathBuf::from("bad\0kernel")] {
        let (_directory, vm) = vm_dir();
        let runtime = Runtime {
            kernel,
            ..cold_runtime()
        };
        let error = create(&runtime, &cold_config(), None, &vm).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
        assert!(error.to_string().contains("Runtime.kernel"), "{error}");
        assert_eq!(fs::read_dir(vm.path()).unwrap().count(), 0);
    }
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn spawn_returns_the_same_outputs_and_capability_with_a_verified_process() {
    let (_directory, vm) = vm_dir();
    let prepared = stand_in(&vm);
    let outputs = prepared.outputs().clone();
    let capability = *prepared.capability().as_bytes();
    let launched = RunningVm(spawn(prepared).unwrap());
    assert_eq!(launched.0.outputs, outputs);
    assert_eq!(launched.0.capability.as_bytes(), &capability);
    assert!(launched.0.ready.is_none());
    let reference = launched.0.process.reference();
    assert_ne!(reference.pid, std::process::id());
    let Opened::Running(reopened) = Process::open(reference).unwrap() else {
        panic!("the recorded VMM runs");
    };
    assert_eq!(reopened.reference(), reference);
    assert!(!reopened.has_exited().unwrap());
    assert!(!format!("{:?}", launched.0).contains(&format!("{capability:?}")));
    assert_eq!(fs::read_dir(vm.path()).unwrap().count(), 1);
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn a_failure_after_child_creation_terminates_only_that_child_and_confirms_its_exit() {
    let (_directory, vm) = vm_dir();
    let (_foreign_directory, foreign_vm) = vm_dir();
    let foreign = RunningVm(spawn(stand_in(&foreign_vm)).unwrap());
    let mut process = None;
    let error = spawn_verified(stand_in(&vm), |child| {
        process = Some(child.process().unwrap());
        assert!(!process.as_ref().unwrap().has_exited().unwrap());
        Err(io::Error::other("injected process verification failure"))
    })
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert_eq!(
        std::error::Error::source(&error).unwrap().to_string(),
        "injected process verification failure"
    );
    assert!(process.unwrap().has_exited().unwrap());
    assert!(!foreign.0.process.has_exited().unwrap());
    assert!(vm.path().join(LOG).exists());
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn a_failed_verification_with_unconfirmed_exit_keeps_its_typed_cleanup_refusal() {
    let (_directory, vm) = vm_dir();
    let mut process = None;
    let error = spawn_verified_with_cleanup(
        stand_in(&vm),
        |child| {
            process = Some(child.process().unwrap());
            Err(io::Error::other("injected process verification failure"))
        },
        |child| {
            child.kill_and_confirm_exit().unwrap();
            Err(io::Error::other("injected missing exit confirmation"))
        },
    )
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::StillRunning);
    assert_eq!(
        std::error::Error::source(&error).unwrap().to_string(),
        "injected process verification failure"
    );
    assert!(
        error
            .to_string()
            .contains("injected missing exit confirmation")
    );
    assert!(vm.path().join(LOG).exists());
    assert!(process.unwrap().has_exited().unwrap());
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn spawn_failure_keeps_caller_owned_files_and_reports_the_host_error() {
    let (_directory, vm) = vm_dir();
    let mut prepared = stand_in(&vm);
    prepared.program = vm.path().join("absent-openvmm");
    let input = vm.path().join("caller-input");
    fs::write(&input, b"caller-owned").unwrap();
    let error = spawn(prepared).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    let source = std::error::Error::source(&error)
        .unwrap()
        .downcast_ref::<io::Error>()
        .unwrap();
    assert_eq!(source.kind(), io::ErrorKind::NotFound);
    assert_eq!(fs::read(&input).unwrap(), b"caller-owned");
    assert!(vm.path().join(LOG).exists());
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn spawn_passes_the_opened_log_and_owned_resources_without_reopening_paths() {
    let (_directory, vm) = vm_dir();
    let mut prepared = stand_in(&vm);
    let original_log = vm.path().join("original-log");
    fs::rename(&prepared.outputs.log, &original_log).unwrap();
    fs::write(&prepared.outputs.log, b"replacement-log").unwrap();
    let input = vm.path().join("caller-input");
    let file = File::create(&input).unwrap();
    let retained = file.try_clone().unwrap();
    #[cfg(target_os = "linux")]
    let value = std::os::fd::AsRawFd::as_raw_fd(&file);
    #[cfg(windows)]
    let value = std::os::windows::io::AsRawHandle::as_raw_handle(&file) as usize;
    prepared.inherit(file);
    let original_input = vm.path().join("original-input");
    fs::rename(&input, &original_input).unwrap();
    fs::write(&input, b"replacement-input").unwrap();
    let launched = RunningVm(
        spawn_verified(prepared, |child| {
            #[cfg(target_os = "linux")]
            {
                let info = fs::read_to_string(format!("/proc/self/fdinfo/{value}")).unwrap();
                let flags = info
                    .lines()
                    .find_map(|line| line.strip_prefix("flags:"))
                    .map(|flags| u32::from_str_radix(flags.trim(), 8).unwrap())
                    .unwrap();
                assert_ne!(flags & nix::libc::O_CLOEXEC as u32, 0);
            }
            child.process()
        })
        .unwrap(),
    );
    let pid = launched.0.process.reference().pid;
    #[cfg(target_os = "linux")]
    {
        let object = |descriptor| fs::read_link(format!("/proc/{pid}/fd/{descriptor}")).unwrap();
        assert_eq!(object(value), original_input);
        assert_eq!(object(1), original_log);
        assert_eq!(object(2), original_log);
        drop(retained);
    }
    #[cfg(windows)]
    {
        assert!(platform::tests::inherits_file(pid, value, &retained));
        let deadline = Instant::now() + Duration::from_secs(10);
        while fs::metadata(&original_log).unwrap().len() == 0 {
            assert!(
                Instant::now() < deadline,
                "the opened log received no output"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    assert_eq!(fs::read(&input).unwrap(), b"replacement-input");
    assert_eq!(
        fs::read(&launched.0.outputs.log).unwrap(),
        b"replacement-log"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn library_spawns_preserve_the_callers_umask_and_clear_only_the_childs_owner_bits() {
    let umask_in = |path: &str| {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("Umask:"))
            .map(|mask| u32::from_str_radix(mask.trim(), 8).unwrap())
            .unwrap()
    };
    for mask in [
        0o002, 0o022, 0o027, 0o077, 0o100, 0o200, 0o400, 0o477, 0o777,
    ] {
        let (_directory, vm) = vm_dir();
        let launched = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    nix::sched::unshare(nix::sched::CloneFlags::CLONE_FS).unwrap();
                    nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(mask));
                    let launched = RunningVm(spawn(stand_in(&vm)).unwrap());
                    assert_eq!(umask_in("/proc/thread-self/status"), mask);
                    launched
                })
                .join()
                .unwrap()
        });
        assert_eq!(
            umask_in(&format!(
                "/proc/{}/status",
                launched.0.process.reference().pid
            )),
            mask & 0o077
        );
    }
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn a_library_vm_outlives_the_caller_that_spawned_it_without_retaining_its_output_pipe() {
    let directory = tempfile::tempdir().unwrap();
    let vm = directory.path().join("vm");
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "host::vm::tests::spawn_caller_helper",
            "--ignored",
            "--nocapture",
        ])
        .env("NVX_VM_SPAWN_CALLER_DIR", &vm)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reference =
        serde_json::from_slice(&fs::read(vm.join("caller-process.json")).unwrap()).unwrap();
    let Opened::Running(process) = Process::open(&reference).unwrap() else {
        panic!("the caller's recorded VMM runs");
    };
    let launched = RunningVm(Launched {
        process,
        outputs: VmDir::open(&vm).unwrap().outputs(false).unwrap(),
        capability: Capability([0; CAPABILITY_LEN]),
        ready: None,
        #[cfg(feature = "cli")]
        child: ForegroundChild(None),
    });
    assert!(!launched.0.process.has_exited().unwrap());
}

#[cfg(all(feature = "cli", any(target_os = "linux", windows)))]
#[test]
fn an_owned_child_status_is_kept_when_its_outcome_is_missing_or_malformed() {
    for malformed in [false, true] {
        let (_directory, dir) = vm_dir();
        let mut prepared = stand_in(&dir).for_foreground();
        #[cfg(target_os = "linux")]
        {
            prepared.program = PathBuf::from("/bin/sh");
            prepared.arguments = ["-c", "exit 37"].map(OsString::from).to_vec();
        }
        #[cfg(windows)]
        {
            prepared.program = PathBuf::from(std::env::var_os("ComSpec").unwrap());
            prepared.arguments = ["/d", "/c", "exit 37"].map(OsString::from).to_vec();
        }
        let mut launched = RunningVm(spawn(prepared).unwrap());
        assert_eq!(
            launched.0.wait_status(Duration::from_secs(10)).unwrap(),
            Some(37)
        );
        if malformed {
            fs::write(&launched.0.outputs.outcome, b"not-json").unwrap();
        }
        let now = Instant::now();
        let stopped = stop(
            &launched.0.process,
            &launched.0.outputs,
            |_| false,
            now,
            now,
        )
        .unwrap();
        assert!(stopped.self_exited);
        assert_eq!(stopped.report.is_err(), malformed);
        assert_eq!(launched.0.wait_status(Duration::ZERO).unwrap(), Some(37));
    }
}

#[cfg(any(target_os = "linux", windows))]
#[test]
#[ignore = "invoked in a separate caller by the caller-survival test"]
fn spawn_caller_helper() {
    let path = PathBuf::from(std::env::var_os("NVX_VM_SPAWN_CALLER_DIR").unwrap());
    create_private_dir(&path).unwrap();
    let dir = VmDir::open(&path).unwrap();
    let launched = spawn(stand_in(&dir)).unwrap();
    let reference = serde_json::to_vec(launched.process.reference()).unwrap();
    if let Err(error) = fs::write(path.join("caller-process.json"), reference) {
        launched.process.kill().unwrap();
        assert!(
            launched
                .process
                .wait(Instant::now() + Duration::from_secs(10))
                .unwrap()
                .is_some()
        );
        panic!("cannot record the helper's child: {error}");
    }
    drop(launched);
}

#[cfg(windows)]
#[test]
fn library_spawns_refuse_taken_and_untrusted_log_job_names() {
    for trusted in [true, false] {
        let (_directory, vm) = vm_dir();
        let prepared = stand_in(&vm);
        let held = if trusted {
            platform::create_log_job(prepared.log()).unwrap().unwrap()
        } else {
            platform::tests::untrusted_log_job(prepared.log())
        };
        let error =
            spawn_verified(prepared, |_| panic!("a refused job started a child")).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io);
        let source = std::error::Error::source(&error)
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap();
        assert_eq!(
            source.kind(),
            if trusted {
                io::ErrorKind::AlreadyExists
            } else {
                io::ErrorKind::PermissionDenied
            }
        );
        drop(held);
    }
}

#[cfg(all(windows, feature = "cli"))]
#[test]
fn an_unrelated_process_started_in_the_library_spawn_window_gets_no_vm_claim() {
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let (_directory, vm) = vm_dir();
    let prepared = stand_in(&vm);
    let log = prepared.outputs.log.clone();
    let identity = platform::file_identity(&log).unwrap();
    let program = prepared.program.clone();
    let arguments = prepared.arguments.clone();
    let (entered, in_window) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        platform::tests::with_spawn_window(
            move || {
                entered.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(10)).unwrap();
            },
            || spawn(prepared),
        )
    });
    in_window.recv_timeout(Duration::from_secs(10)).unwrap();
    let mut other = ChildGuard(
        Command::new(program)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    release.send(()).unwrap();
    let launched = RunningVm(thread.join().unwrap().unwrap());
    assert!(platform::log_claim_held(&log, identity).unwrap());
    launched.0.process.kill().unwrap();
    assert!(
        launched
            .0
            .process
            .wait(Instant::now() + Duration::from_secs(10))
            .unwrap()
            .is_some()
    );
    assert!(
        platform::tests::claim_released_within(&log, identity, Duration::from_secs(10)),
        "an unrelated process retained the VM's job claim"
    );
    assert!(other.0.try_wait().unwrap().is_none());
}

#[cfg(any(target_os = "linux", windows))]
#[test]
#[ignore = "needs exact current OpenVMM, kernel and initramfs paths and an available hypervisor"]
fn current_cold_spawn_boots_a_guest_accepts_shell_input_and_exits_cleanly() {
    use sha2::{Digest, Sha256};

    let path =
        |name| PathBuf::from(std::env::var_os(name).expect("missing explicit VM proof input"));
    let openvmm = path("NVX_VM_SPAWN_OPENVMM");
    let kernel = path("NVX_VM_SPAWN_KERNEL");
    let initramfs = path("NVX_VM_SPAWN_INITRAMFS");
    let identity = |path: &Path| {
        ArtifactIdentity::sha256(&format!("{:x}", Sha256::digest(fs::read(path).unwrap()))).unwrap()
    };
    let runtime = Runtime {
        identity: RuntimeIdentity::new(
            identity(&openvmm),
            identity(&kernel),
            identity(&initramfs),
            test_compatibility(),
        ),
        openvmm,
        kernel,
        initramfs,
        hypervisor: Hypervisor::from_env_or_default().unwrap(),
        cpu_profile: Some("auto".to_owned()),
        ..cold_runtime()
    };
    let path = path("NVX_VM_SPAWN_PROOF_DIR");
    assert!(
        !path.exists(),
        "a cold-spawn proof needs a fresh owned directory"
    );
    create_private_dir(&path).unwrap();
    let vm = VmDir::open(&path).unwrap();
    let config = VmConfig {
        boot: Some(Boot {
            memory_capacity_mib: None,
            command_line: "quiet loglevel=0".to_owned(),
            capture: None,
        }),
        ..cold_config()
    };
    let prepared = create(&runtime, &config, None, &vm).unwrap();
    let capability = *prepared.capability().as_bytes();
    let launched = RunningVm(spawn(prepared).unwrap());
    assert_eq!(launched.0.capability.as_bytes(), &capability);
    let reference = launched.0.process.reference();
    let Opened::Running(reopened) = Process::open(reference).unwrap() else {
        panic!("the recorded VMM runs");
    };
    assert_eq!(reopened.reference(), reference);
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut console = loop {
        match platform::connect_endpoint(
            launched.0.outputs.console.as_deref().unwrap(),
            reference.pid,
            Duration::from_millis(50),
        ) {
            Ok(console) => break console,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound
                        | io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::WouldBlock
                ) && Instant::now() < deadline =>
            {
                assert!(!launched.0.process.has_exited().unwrap());
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => panic!("cannot attach the spawned VM's console: {error}"),
        }
    };
    let read_until = |console: &mut dyn platform::Transport, marker: &[u8]| {
        let mut received = Vec::new();
        while !received.windows(marker.len()).any(|part| part == marker) {
            assert!(
                Instant::now() < deadline,
                "guest output: {}",
                String::from_utf8_lossy(&received)
            );
            let mut buffer = [0; 4096];
            match console.read(&mut buffer, Some(Duration::from_millis(100))) {
                Ok(0) => panic!("console ended before {}", String::from_utf8_lossy(marker)),
                Ok(count) => received.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == io::ErrorKind::TimedOut => {}
                Err(error) => panic!("cannot read the guest console: {error}"),
            }
            assert!(received.len() <= 1024 * 1024);
        }
        println!("{}", String::from_utf8_lossy(&received));
    };
    read_until(&mut *console, b"NVX-GUEST-BOOT-OK: alpine");
    console
        .write_all(
            b"printf 'NVX-SPAWN-COLD-%s\\n' OK; /sbin/nvx-exit 0\n",
            Some(Duration::from_secs(5)),
        )
        .unwrap();
    read_until(&mut *console, b"NVX-SPAWN-COLD-OK");
    assert!(launched.0.process.wait(deadline).unwrap().is_some());
    assert!(reopened.has_exited().unwrap());
    let outcome = crate::host::report::read(&launched.0.outputs.outcome)
        .unwrap()
        .unwrap();
    assert_eq!(outcome.category.as_deref(), Some("success"));
    assert_eq!(outcome.status_code, Some(0));
    println!("NVX-LIBRARY-COLD-SPAWN-EXIT-OK: {}", reference.pid);
}

#[cfg(any(target_os = "linux", windows))]
struct StopChild {
    child: std::process::Child,
    process: Process,
}

#[cfg(any(target_os = "linux", windows))]
impl StopChild {
    fn start() -> Self {
        use std::io::{BufRead, BufReader};

        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new(
                PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32\\cmd.exe"),
            );
            command.args(["/d", "/q", "/c", "echo ready & set /p wait="]);
            command
        };
        #[cfg(target_os = "linux")]
        let mut command = {
            let mut command = Command::new("sh");
            command.args(["-c", "printf 'ready\\n'; read -r wait"]);
            command
        };
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = crate::host::tests::spawn_locked(&mut command).unwrap();
        let mut ready = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready.trim_end(), "ready");
        let reference = crate::host::process::ProcessRef {
            pid: child.id(),
            start_time: platform::process_start_time(child.id()).unwrap().unwrap(),
            boot: crate::host::process::BootId::current().unwrap(),
        };
        let Opened::Running(process) = Process::open(&reference).unwrap() else {
            panic!("the owned child runs");
        };
        Self { child, process }
    }
}

#[cfg(any(target_os = "linux", windows))]
impl Drop for StopChild {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
        }
        self.child.wait().unwrap();
    }
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn stop_preserves_report_results_after_a_real_self_exit() {
    for report in ["missing", "valid", "malformed", "unreadable"] {
        let (_directory, vm) = vm_dir();
        let outputs = vm.outputs(false).unwrap();
        match report {
            "missing" => {}
            "valid" => fs::write(
                &outputs.outcome,
                br#"{"outcome":{"category":"success","status_code":0},"teardown":{"workers":true}}"#,
            ).unwrap(),
            "malformed" => fs::write(&outputs.outcome, b"{ truncated").unwrap(),
            "unreadable" => fs::create_dir(&outputs.outcome).unwrap(),
            _ => unreachable!(),
        }
        let mut child = StopChild::start();
        let deadline = Instant::now() + Duration::from_secs(10);
        let stopped = stop(
            &child.process,
            &outputs,
            |supplied| {
                assert_eq!(supplied, deadline);
                drop(child.child.stdin.take());
                false
            },
            deadline,
            deadline,
        )
        .unwrap();
        assert!(stopped.self_exited);
        assert!(!stopped.forced);
        assert!(child.process.wait(Instant::now()).unwrap().is_some());
        assert!(child.child.try_wait().unwrap().is_some());
        let already_exited = stop(
            &child.process,
            &outputs,
            |_| panic!("an exited process needs no graceful request"),
            Instant::now(),
            Instant::now(),
        )
        .unwrap();
        assert!(already_exited.self_exited);
        assert!(!already_exited.forced);
        match report {
            "missing" => assert!(stopped.report.unwrap().is_none()),
            "valid" => assert_eq!(
                stopped.report.unwrap(),
                Some(Report {
                    category: Some("success".to_owned()),
                    status_code: Some(0),
                    teardown_complete: Some(true),
                })
            ),
            _ => {
                let error = stopped.report.unwrap_err();
                assert_eq!(error.kind(), ErrorKind::Io);
                assert!(std::error::Error::source(&error).is_some());
            }
        }
    }
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn stop_forces_only_its_verified_process_after_grace() {
    let (_directory, vm) = vm_dir();
    let outputs = vm.outputs(false).unwrap();
    let mut child = StopChild::start();
    let mut other = StopChild::start();
    let grace = Instant::now() + Duration::from_millis(80);
    let deadline = grace + Duration::from_secs(10);
    let stopped = stop(
        &child.process,
        &outputs,
        |supplied| {
            assert_eq!(supplied, grace);
            true
        },
        grace,
        deadline,
    )
    .unwrap();
    assert!(!stopped.self_exited);
    assert!(stopped.forced);
    assert!(stopped.report.unwrap().is_none());
    assert!(child.process.wait(Instant::now()).unwrap().is_some());
    assert!(child.child.try_wait().unwrap().is_some());
    assert!(other.process.wait(Instant::now()).unwrap().is_none());
    assert!(other.child.try_wait().unwrap().is_none());
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn stop_returns_still_running_without_exit_confirmation_at_its_deadline() {
    let (_directory, vm) = vm_dir();
    let outputs = vm.outputs(false).unwrap();
    fs::write(&outputs.outcome, b"{ truncated").unwrap();
    let mut child = StopChild::start();
    #[cfg(target_os = "linux")]
    let handle = platform::ProcessHandle::without_pidfd();
    #[cfg(windows)]
    let handle = platform::ProcessHandle::wait_only(child.child.id());
    let process = Process::held(child.process.reference().clone(), handle);
    let grace = Instant::now();
    let deadline = grace + Duration::from_millis(80);
    let error = stop(&process, &outputs, |_| false, grace, deadline).unwrap_err();
    assert!(Instant::now() >= deadline);
    assert_eq!(error.kind(), ErrorKind::StillRunning);
    assert_eq!(error.code().as_str(), "still-running");
    let cause = std::error::Error::source(&error)
        .unwrap()
        .downcast_ref::<io::Error>()
        .unwrap();
    assert_eq!(
        cause.kind(),
        if cfg!(windows) {
            io::ErrorKind::PermissionDenied
        } else {
            io::ErrorKind::Unsupported
        }
    );
    assert!(process.wait(Instant::now()).unwrap().is_none());
    assert!(child.child.try_wait().unwrap().is_none());
}
