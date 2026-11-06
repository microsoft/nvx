//! Tests of a VM's configuration and of its check, on paths that don't exist, since the check
//! reads nothing on the host.

use std::error::Error as _;

use super::*;
use crate::ErrorKind;
use crate::activation::MachineIdMode;
use crate::host::artifacts::Artifacts;
use crate::host::filesystem::{Bind, ExportChild, Owner};
use crate::host::launch::LaunchSpec;
use crate::host::network::Access;
use crate::host::sandbox::{SandboxDistro, SandboxPlan, SandboxSettings, SandboxUpper};
use crate::hypervisor::Hypervisor;
use crate::protocol::{cmdline, parse_host_mapping_count};
use crate::uuid::Uuid;

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
        }),
        Some(Boot {
            command_line: "quiet loglevel=0".to_owned(),
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
fn caller_tokens_follow_rendered_tokens_without_any_rewriting() {
    let caller = " \tquiet nvx_entrypoint=/bin/work nvx_arg=\"x y\" nvx_share=0,/work\r\n ";
    let mut config = VmConfig {
        boot: Some(Boot {
            command_line: String::new(),
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
        }),
        ..config
    };
    let error = config.kernel_command_line().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    assert!(error.to_string().contains("processor"), "{error}");
}
