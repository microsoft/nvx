//! Tests of activation application through an assembly, export, and VM-wide view seam.
//! The seam has no process or admission operation.

use std::error::Error as _;
use std::path::Path;

use super::*;
use crate::guest::restore::{
    GuestRestore, GuestRestoreAction, GuestRestoreCause, GuestRestoreInput, GuestRestoreState,
};
use crate::guest::root::tests::assembly_failure;
use crate::guest::sys::Errno;
use crate::guest_restore::{
    ExpectedResources, Export, ExportKind, Network, RestoreSection, TransactionId,
};
use crate::protocol::root::{DISTRO_MMIO_BASE, SCRATCH_MMIO_BASE};
use crate::protocol::view::{MachineIdMode, View};
use crate::uuid::Uuid;

/// The test template's runtime directory.
const RUNTIME: &str = "/run/nvx";
/// The test template's filesystem tag.
const TAG: &str = "microvm";
/// The test template's gateway.
const GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);
/// A DNS server deliberately different from the gateway.
const DNS: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 53));
/// The test records' proxy endpoint.
const PROXY: &str = "http://10.0.0.1:3128";
/// The test records' hostname.
const HOSTNAME: &[u8] = b"clone";
/// The resources reported by the repaired clone.
const RESOURCES: ExpectedResources = ExpectedResources {
    cpus: 2,
    memory: 1 << 30,
};
/// The tests' transaction.
const TRANSACTION: TransactionId = TransactionId::from_bytes([0x22; TransactionId::LEN]);
/// The expected EROFS UUID.
const IMAGE_UUID: Uuid = Uuid::from_bytes([0x5a; 16]);
/// The expected GPT disk GUID.
const DISK_GUID: Uuid = Uuid::from_bytes([0xd1; 16]);

/// Every system operation the application can perform.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Effect {
    /// Reread this distro device's partition table.
    Reread(Device),
    /// Assemble this root with a fresh upper.
    Assemble(Assembly),
    /// Mount the export through this tag with this access.
    MountExport(String, Access),
    /// Apply this exact VM-wide view and DNS server.
    View(View, Option<IpAddr>),
    /// Close the export descriptor.
    Close,
    /// Tear down the root, including its view binds.
    Teardown,
}

/// Records all calls and returns a temporary directory in place of a mounted root.
struct FakeSystem {
    /// The assembled root's directory.
    image: tempfile::TempDir,
    /// Whether assembly fails.
    failing: bool,
    /// Whether rereading the partition table fails.
    refusing: bool,
    /// Whether mounting the export fails.
    unmountable: bool,
    /// Whether applying the VM-wide view fails.
    unviewable: bool,
    /// All system calls, in order.
    effects: Vec<Effect>,
    /// The deadline passed to each device wait.
    deadlines: Vec<Option<Instant>>,
}

impl FakeSystem {
    /// Returns an initially successful system.
    fn new() -> Self {
        Self {
            image: tempfile::tempdir().unwrap(),
            failing: false,
            refusing: false,
            unmountable: false,
            unviewable: false,
            effects: Vec::new(),
            deadlines: Vec::new(),
        }
    }

    /// Returns the assembled root's path.
    fn root(&self) -> PathBuf {
        self.image.path().to_owned()
    }
}

impl System for FakeSystem {
    type Root = PathBuf;
    type Export = ();

    fn reread(
        &mut self,
        distro: Device,
        _timeout: Duration,
        deadline: Option<Instant>,
    ) -> Result<(), root::Error> {
        self.effects.push(Effect::Reread(distro));
        self.deadlines.push(deadline);
        if self.refusing {
            return Err(root::Error::Device {
                role: root::Role::Distro,
                step: root::DeviceStep::Reread,
                device: "/dev/vda".into(),
                error: Errno::EBUSY.into(),
            });
        }
        Ok(())
    }

    fn assemble(
        &mut self,
        assembly: &Assembly,
        deadline: Option<Instant>,
    ) -> Result<PathBuf, AssemblyError> {
        self.effects.push(Effect::Assemble(assembly.clone()));
        self.deadlines.push(deadline);
        if self.failing {
            return Err(root::assemble(&Assembly {
                runtime: "run/nvx".into(),
                ..assembly.clone()
            })
            .unwrap_err());
        }
        Ok(self.root())
    }

    fn mount_export(
        &mut self,
        root: &mut PathBuf,
        tag: &str,
        access: Access,
    ) -> Result<(), root::Error> {
        assert_eq!(*root, self.root());
        self.effects
            .push(Effect::MountExport(tag.to_owned(), access));
        if self.unmountable {
            return Err(root::Error::Mount {
                mount: root::Mount::Export {
                    tag: tag.to_owned(),
                },
                error: Errno::ENOENT.into(),
            });
        }
        Ok(())
    }

    fn close(&mut self, (): ()) {
        self.effects.push(Effect::Close);
    }

    fn view(
        &mut self,
        root: &mut PathBuf,
        view: &View,
        dns: Option<IpAddr>,
    ) -> Result<MachineId, view::ApplyFailure> {
        assert_eq!(*root, self.root());
        self.effects.push(Effect::View(view.clone(), dns));
        if self.unviewable {
            return Err(crate::Error::io("the VM view failed")
                .with_source(io::Error::from(Errno::EACCES))
                .into());
        }
        Ok(match view.machine_id {
            MachineIdMode::Image => MachineId::Image,
            MachineIdMode::Generated => {
                MachineId::Generated(Path::new(RUNTIME).join(MACHINE_ID_FILE))
            }
        })
    }

    fn teardown(&mut self, root: PathBuf) -> Result<(), TeardownError> {
        assert_eq!(root, self.root());
        self.effects.push(Effect::Teardown);
        Ok(())
    }
}

/// Returns the template's devices, runtime, deadline, and gateway.
fn template() -> RootTemplate {
    RootTemplate {
        runtime: RUNTIME.into(),
        distro: Device {
            mmio: DISTRO_MMIO_BASE,
            sectors: Some(1 << 21),
        },
        scratch: Device {
            mmio: SCRATCH_MMIO_BASE,
            sectors: Some(1 << 22),
        },
        timeout: Duration::from_secs(5),
        filesystem_tag: TAG.to_owned(),
        gateway: Some(GATEWAY),
    }
}

/// Returns the tests' basic configuration, without an export or network.
fn configuration() -> Configuration {
    Configuration {
        image: guest_restore::Image::Erofs {
            uuid: Some(IMAGE_UUID),
        },
        upper: guest_restore::Upper::Tmpfs {
            max_memory_percent: 50,
        },
        overlay: OverlayMode::UntrustedLowers,
        export: None,
        mappings: 0,
        network: None,
        view: View {
            hostname: HOSTNAME.to_vec(),
            machine_id: MachineIdMode::Image,
        },
        resources: RESOURCES,
    }
}

/// Returns a clone's activation record.
fn clone_record(configuration: Configuration) -> GuestRestoreRecord {
    GuestRestoreRecord::new(
        Some(RestoreSection {
            transaction: TRANSACTION,
        }),
        configuration,
    )
    .unwrap()
}

/// Returns the tests' basic accepted record.
fn accepted() -> GuestRestoreRecord {
    clone_record(configuration())
}

/// Returns a record with every optional application input.
fn with_everything() -> GuestRestoreRecord {
    let mut configuration = configuration();
    configuration.view.machine_id = MachineIdMode::Generated;
    configuration.export = Some(Export {
        kind: ExportKind::Aggregate,
    });
    configuration.mappings = 3;
    configuration.network = Some(Network {
        dns: Some(DNS),
        publication: Publication::Published,
        proxy: Some(PROXY.to_owned()),
    });
    clone_record(configuration)
}

/// Returns the expected assembly of the basic accepted record.
fn accepted_assembly() -> Assembly {
    Assembly {
        runtime: RUNTIME.into(),
        distro: template().distro,
        image: root::Image::Erofs {
            uuid: Some(IMAGE_UUID),
        },
        upper: root::Upper::Tmpfs {
            max_memory_percent: Some(50),
        },
        overlay: root::OverlayMode::UntrustedLowers,
        timeout: template().timeout,
    }
}

/// Returns an activation whose transport is restricted and ready for one Activate.
fn restricted(now: Instant) -> GuestRestore {
    let mut activation =
        GuestRestore::with_resources(RESOURCES, Some(now), Duration::from_secs(10), now);
    assert_eq!(
        activation.input(now, GuestRestoreInput::Reset).unwrap(),
        GuestRestoreAction::Acknowledge,
    );
    activation
}

#[test]
fn no_mount_spawn_or_id_generation_happens_before_the_gate() {
    let now = Instant::now();
    let bytes = with_everything().encode();
    let cold = GuestRestoreRecord::new(None, configuration())
        .unwrap()
        .encode();
    let mut wrong = configuration();
    wrong.resources.cpus += 1;
    let wrong = clone_record(wrong).encode();
    for offered in [&bytes[..bytes.len() - 1], &cold, &wrong] {
        let mut activation = restricted(now);
        assert!(
            activation
                .input(now, GuestRestoreInput::Activate(offered))
                .is_err()
        );
        assert_eq!(activation.state(), GuestRestoreState::Failed);
    }
    let mut activation = restricted(now);
    assert!(matches!(
        activation
            .input(
                now + Duration::from_secs(10),
                GuestRestoreInput::Activate(&bytes)
            )
            .unwrap_err()
            .cause(),
        GuestRestoreCause::Expired,
    ));
}

#[test]
fn a_valid_activate_activates_the_root_and_the_workload_once_before_ready() {
    let now = Instant::now();
    let record = with_everything();
    let bytes = record.encode();
    let mut activation = restricted(now);
    let GuestRestoreAction::Apply(accepted) = activation
        .input(now, GuestRestoreInput::Activate(&bytes))
        .unwrap()
    else {
        panic!("a valid record was not accepted")
    };
    let mut system = FakeSystem::new();
    let applied = apply_with(&mut system, &accepted, &template(), activation.deadline()).unwrap();
    let expected = vec![
        Effect::Assemble(accepted_assembly()),
        Effect::MountExport(TAG.to_owned(), Access::ReadWrite),
        Effect::View(record.configuration().view.clone(), Some(DNS)),
    ];
    assert_eq!(system.effects, expected);
    assert_eq!(applied.proxy.as_deref(), Some(PROXY));
    assert_eq!(applied.publication, Publication::Published);
    assert_eq!(applied.gateway, Some(GATEWAY));
    let ready = activation.complete(now, Ok(())).unwrap();
    assert_eq!(ready, crate::guest_restore::GuestRestoreReady::of(&record));
    for _ in 0..2 {
        assert_eq!(
            activation.input(now, GuestRestoreInput::Reset).unwrap(),
            GuestRestoreAction::Acknowledge
        );
        assert_eq!(
            activation
                .input(now, GuestRestoreInput::Activate(&bytes))
                .unwrap(),
            GuestRestoreAction::Ready(ready)
        );
    }
    assert_eq!(system.effects, expected);
    assert_eq!(
        activation.input(now, GuestRestoreInput::Request).unwrap(),
        GuestRestoreAction::Serve
    );
}

#[test]
fn assembly_inputs_are_taken_exactly_from_the_record() {
    let images = [
        (
            guest_restore::Image::Erofs { uuid: None },
            root::Image::Erofs { uuid: None },
            false,
        ),
        (
            guest_restore::Image::Erofs {
                uuid: Some(IMAGE_UUID),
            },
            root::Image::Erofs {
                uuid: Some(IMAGE_UUID),
            },
            false,
        ),
        (
            guest_restore::Image::Gpt {
                disk_guid: None,
                first_partition_bound: 4096,
            },
            root::Image::Gpt {
                expected: Expected::new(),
                first_partition_bound: 4096,
            },
            true,
        ),
        (
            guest_restore::Image::Gpt {
                disk_guid: Some(DISK_GUID),
                first_partition_bound: 4096,
            },
            root::Image::Gpt {
                expected: Expected::new().disk_guid(DISK_GUID),
                first_partition_bound: 4096,
            },
            true,
        ),
    ];
    let uppers = [
        (
            guest_restore::Upper::Tmpfs {
                max_memory_percent: 1,
            },
            root::Upper::Tmpfs {
                max_memory_percent: Some(1),
            },
        ),
        (
            guest_restore::Upper::Tmpfs {
                max_memory_percent: 90,
            },
            root::Upper::Tmpfs {
                max_memory_percent: Some(90),
            },
        ),
        (
            guest_restore::Upper::Scratch {
                journal: Journal::Allowed,
            },
            root::Upper::Scratch {
                device: template().scratch,
                journal: root::Journal::Allowed,
            },
        ),
        (
            guest_restore::Upper::Scratch {
                journal: Journal::Forbidden,
            },
            root::Upper::Scratch {
                device: template().scratch,
                journal: root::Journal::Forbidden,
            },
        ),
    ];
    let overlays = [
        (OverlayMode::TrustedLowers, root::OverlayMode::TrustedLowers),
        (
            OverlayMode::UntrustedLowers,
            root::OverlayMode::UntrustedLowers,
        ),
    ];
    for (image, expected_image, reread) in images {
        for (upper, expected_upper) in uppers {
            for (overlay, expected_overlay) in overlays {
                let record = clone_record(Configuration {
                    image,
                    upper,
                    overlay,
                    ..configuration()
                });
                let mut system = FakeSystem::new();
                let applied = apply_with(&mut system, &record, &template(), None).unwrap();
                let expected = Assembly {
                    image: expected_image.clone(),
                    upper: expected_upper,
                    overlay: expected_overlay,
                    ..accepted_assembly()
                };
                let mut effects = Vec::new();
                if reread {
                    effects.push(Effect::Reread(template().distro));
                }
                effects.push(Effect::Assemble(expected));
                effects.push(Effect::View(record.configuration().view.clone(), None));
                assert_eq!(system.effects, effects, "{record:?}");
                assert_eq!(applied.root, system.root());
            }
        }
    }
    let first = with_everything();
    let mut other = first.configuration().clone();
    other.export = Some(Export {
        kind: ExportKind::Directory,
    });
    other.mappings = 7;
    other.resources = ExpectedResources {
        cpus: 8,
        memory: 1 << 33,
    };
    let other = GuestRestoreRecord::new(None, other).unwrap();
    let (mut first_system, mut other_system) = (FakeSystem::new(), FakeSystem::new());
    apply_with(&mut first_system, &first, &template(), None).unwrap();
    apply_with(&mut other_system, &other, &template(), None).unwrap();
    assert_eq!(
        first_system.effects,
        [
            Effect::Assemble(accepted_assembly()),
            Effect::MountExport(TAG.to_owned(), Access::ReadWrite),
            Effect::View(first.configuration().view.clone(), Some(DNS)),
        ]
    );
    assert_eq!(first_system.effects, other_system.effects);
}

#[test]
fn a_gpt_disk_s_partition_table_is_reread_before_its_root_is_assembled() {
    let record = clone_record(Configuration {
        image: guest_restore::Image::Gpt {
            disk_guid: Some(DISK_GUID),
            first_partition_bound: 4096,
        },
        ..configuration()
    });
    let deadline = Some(Instant::now() + Duration::from_secs(10));
    let mut system = FakeSystem::new();
    apply_with(&mut system, &record, &template(), deadline).unwrap();
    assert!(matches!(
        system.effects.as_slice(),
        [Effect::Reread(_), Effect::Assemble(_), Effect::View(..)]
    ));
    assert_eq!(system.deadlines, [deadline, deadline]);
    let mut refusing = FakeSystem {
        refusing: true,
        ..FakeSystem::new()
    };
    let error = apply_with(&mut refusing, &record, &template(), deadline).unwrap_err();
    assert!(matches!(
        error,
        ApplyError::Gpt(root::Error::Device {
            step: root::DeviceStep::Reread,
            ..
        })
    ));
    assert_eq!(refusing.effects, [Effect::Reread(template().distro)]);
}

#[test]
fn the_application_leaves_the_images_accounts_to_each_execution() {
    for (passwd, group) in [
        (None, None),
        (Some(&b"malformed\n"[..]), Some(&b"malformed\n"[..])),
        (
            Some(&b"root:x:0:0::/root:/bin/sh\n"[..]),
            Some(&b"root:x:0:\n"[..]),
        ),
    ] {
        let mut system = FakeSystem::new();
        let etc = system.image.path().join("etc");
        std::fs::create_dir(&etc).unwrap();
        if let Some(passwd) = passwd {
            std::fs::write(etc.join("passwd"), passwd).unwrap();
        }
        if let Some(group) = group {
            std::fs::write(etc.join("group"), group).unwrap();
        }
        apply_with(&mut system, &accepted(), &template(), None).unwrap();
        assert_eq!(
            system.effects,
            [
                Effect::Assemble(accepted_assembly()),
                Effect::View(configuration().view, None),
            ]
        );
    }
}

#[test]
fn a_failed_assembly_is_final_and_reads_and_tears_down_nothing() {
    let now = Instant::now();
    let mut activation = restricted(now);
    let bytes = accepted().encode();
    activation
        .input(now, GuestRestoreInput::Activate(&bytes))
        .unwrap();
    let mut system = FakeSystem {
        failing: true,
        ..FakeSystem::new()
    };
    let error = apply_with(&mut system, &accepted(), &template(), None).unwrap_err();
    assert!(matches!(&error, ApplyError::Assembly(error) if matches!(
        assembly_failure(error), root::Error::Runtime { path } if path == Path::new("run/nvx")
    )));
    let failure = activation.complete(now, Err(error.into())).unwrap_err();
    assert!(matches!(failure.cause(), GuestRestoreCause::Failed(_)));
    assert!(
        activation
            .input(now, GuestRestoreInput::Activate(&bytes))
            .is_err()
    );
    assert_eq!(system.effects, [Effect::Assemble(accepted_assembly())]);
}

#[test]
fn the_record_s_view_is_applied_once_with_its_own_dns() {
    let ipv6 = IpAddr::V6("fd00::53".parse().unwrap());
    for mode in [MachineIdMode::Image, MachineIdMode::Generated] {
        for dns in [None, Some(DNS), Some(ipv6)] {
            let mut configuration = configuration();
            configuration.view.machine_id = mode;
            configuration.network = Some(Network {
                dns,
                publication: Publication::Unpublished,
                proxy: Some(PROXY.to_owned()),
            });
            let record = clone_record(configuration.clone());
            let mut system = FakeSystem::new();
            let applied = apply_with(&mut system, &record, &template(), None).unwrap();
            assert_eq!(
                system.effects,
                [
                    Effect::Assemble(accepted_assembly()),
                    Effect::View(configuration.view, dns),
                ]
            );
            assert_eq!(applied.proxy.as_deref(), Some(PROXY));
        }
    }
}

#[test]
fn an_export_is_mounted_through_the_template_s_device_and_handed_to_the_adapter() {
    for kind in [ExportKind::Directory, ExportKind::Aggregate] {
        let record = clone_record(Configuration {
            export: Some(Export { kind }),
            mappings: 2,
            ..configuration()
        });
        let mut system = FakeSystem::new();
        let applied = apply_with(&mut system, &record, &template(), None).unwrap();
        assert_eq!(applied.export, Some(()));
        assert_eq!(
            system.effects,
            [
                Effect::Assemble(accepted_assembly()),
                Effect::MountExport(TAG.to_owned(), Access::ReadWrite),
                Effect::View(configuration().view, None),
            ]
        );
    }
    let mut system = FakeSystem::new();
    let applied = apply_with(&mut system, &accepted(), &template(), None).unwrap();
    assert_eq!(applied.export, None);
}

#[test]
fn an_export_or_a_view_that_fails_closes_owned_resources_before_root_teardown() {
    for export in [false, true] {
        let record = if export {
            with_everything()
        } else {
            accepted()
        };
        let mut system = FakeSystem {
            unviewable: true,
            ..FakeSystem::new()
        };
        let error = apply_with(&mut system, &record, &template(), None).unwrap_err();
        let ApplyError::View {
            error,
            teardown: None,
        } = error
        else {
            panic!("{error}")
        };
        assert_eq!(error.kind(), crate::ErrorKind::Io);
        assert_eq!(
            error
                .source()
                .unwrap()
                .downcast_ref::<io::Error>()
                .unwrap()
                .raw_os_error(),
            Some(Errno::EACCES as i32)
        );
        let tail = if export {
            vec![Effect::Close, Effect::Teardown]
        } else {
            vec![Effect::Teardown]
        };
        assert!(system.effects.ends_with(&tail));
        assert_eq!(
            system
                .effects
                .iter()
                .filter(|effect| matches!(effect, Effect::View(..)))
                .count(),
            1
        );
    }
    let mut system = FakeSystem {
        unmountable: true,
        ..FakeSystem::new()
    };
    let error = apply_with(&mut system, &with_everything(), &template(), None).unwrap_err();
    assert!(matches!(error, ApplyError::Export { teardown: None, .. }));
    assert_eq!(
        system.effects,
        [
            Effect::Assemble(accepted_assembly()),
            Effect::MountExport(TAG.to_owned(), Access::ReadWrite),
            Effect::Teardown,
        ]
    );
}

#[test]
fn teardown_closes_the_export_before_the_root_removes_its_own_binds() {
    for export in [None, Some(())] {
        let mut system = FakeSystem::new();
        let root = system.root();
        teardown_with(&mut system, export, root).unwrap();
        let expected = if export.is_some() {
            vec![Effect::Close, Effect::Teardown]
        } else {
            vec![Effect::Teardown]
        };
        assert_eq!(system.effects, expected);
    }
}

#[test]
fn publication_and_gateway_are_reported_without_starting_or_stopping_a_process() {
    for publication in [Publication::Unpublished, Publication::Published] {
        let record = clone_record(Configuration {
            network: Some(Network {
                dns: Some(DNS),
                publication,
                proxy: None,
            }),
            ..configuration()
        });
        let mut system = FakeSystem::new();
        let applied = apply_with(&mut system, &record, &template(), None).unwrap();
        assert_eq!(applied.publication, publication);
        assert_eq!(applied.gateway, Some(GATEWAY));
        assert_eq!(
            system.effects,
            [
                Effect::Assemble(accepted_assembly()),
                Effect::View(configuration().view, Some(DNS)),
            ]
        );
        teardown_with(&mut system, applied.export, applied.root).unwrap();
        assert_eq!(system.effects.last(), Some(&Effect::Teardown));
    }
    let gatewayless = RootTemplate {
        gateway: None,
        ..template()
    };
    let mut system = FakeSystem::new();
    let error = apply_with(&mut system, &with_everything(), &gatewayless, None).unwrap_err();
    assert!(matches!(
        error,
        ApplyError::Network {
            error: NetworkError::Gateway,
            teardown: None
        }
    ));
    assert_eq!(
        system.effects,
        [
            Effect::Assemble(accepted_assembly()),
            Effect::MountExport(TAG.to_owned(), Access::ReadWrite),
            Effect::Close,
            Effect::Teardown,
        ]
    );
    let mut system = FakeSystem::new();
    let applied = apply_with(&mut system, &accepted(), &gatewayless, None).unwrap();
    assert_eq!(applied.publication, Publication::Unpublished);
    assert_eq!(applied.gateway, None);
}

#[test]
fn the_application_refuses_without_privileges_what_it_cannot_assemble() {
    let relative = RootTemplate {
        runtime: "run/nvx".into(),
        ..template()
    };
    let Err(ApplyError::Assembly(error)) = apply(&accepted(), &relative, None) else {
        panic!("a relative runtime directory was assembled")
    };
    assert!(matches!(
        assembly_failure(&error),
        root::Error::Runtime { path } if path == Path::new("run/nvx")
    ));
    let disk = clone_record(Configuration {
        image: guest_restore::Image::Gpt {
            disk_guid: None,
            first_partition_bound: 4096,
        },
        ..configuration()
    });
    let at_once = RootTemplate {
        timeout: Duration::ZERO,
        ..relative
    };
    let Err(ApplyError::Gpt(error)) = apply(&disk, &at_once, None) else {
        panic!("a GPT disk was assembled before its partition table was reread")
    };
    assert!(matches!(
        error,
        root::Error::Missing {
            role: root::Role::Distro,
            mmio: DISTRO_MMIO_BASE
        } | root::Error::Lookup {
            role: root::Role::Distro,
            mmio: DISTRO_MMIO_BASE,
            ..
        }
    ));
}

#[test]
fn application_errors_name_their_cause() {
    let missing = root::Error::Missing {
        role: root::Role::Distro,
        mmio: DISTRO_MMIO_BASE,
    };
    let text = missing.to_string();
    let error = ApplyError::Gpt(missing);
    assert!(error.to_string().ends_with(&text));
    assert_eq!(error.source().unwrap().to_string(), text);
    let error = ApplyError::View {
        error: crate::Error::unsupported("invalid hostname"),
        teardown: None,
    };
    assert!(error.to_string().contains("invalid hostname"));
    assert_eq!(error.source().unwrap().to_string(), "invalid hostname");
    let error = ApplyError::Network {
        error: NetworkError::Gateway,
        teardown: None,
    };
    assert!(error.to_string().contains("template names no IPv4 gateway"));
    assert!(error.source().unwrap().source().is_none());
}

impl ApplyError {
    /// Classifies an actual view failure for the root's retained-bind regression tests.
    pub(crate) fn from_test_view_failure(failure: view::ApplyFailure) -> Self {
        view_failure(failure, None)
    }
}
