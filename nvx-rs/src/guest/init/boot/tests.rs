use super::*;
use crate::ErrorKind;
use crate::activation::MachineIdMode;
use crate::guest::{account as accounts, sys};

const ROOT: &str = "nvx_layer=distro,0xd0003000,erofs nvx_overlay_upper=ramfs";
const VIEW: &str = "nvx_hostname=626f78 nvx_machine_id=generated";
const CLI: &str = "nvx_identity=1000:1001 nvx_control_tty=hvc2";
const EXPORT: &str =
    "virtfs_dir=/run/nvx/hostfs/root virtfs_tag=hostfs virtfs_mode=rw virtfs_aggregate=1";

/// Compiles the steps of the private space that `doc/guest-agent-guidelines.md` quotes, without
/// mounting anything or switching the developer host's root. A change here changes the quote too.
#[test]
fn the_reference_space_steps_compile() {
    use std::error::Error;
    use std::net::IpAddr;
    use std::path::Path;
    use std::time::Instant;

    use crate::activation::{ActivationRecord, View};
    use crate::guest::activation::{self, Applied, RootTemplate};
    use crate::guest::mappings::{Mapper, Reserved, SourceRoot, TargetRoot};
    use crate::guest::root::{self, Assembly, Root};
    use crate::guest::view;
    use crate::protocol::export::HostExport;

    // A cold boot, as `boot::cold_boot` builds it.
    let _cold = |assembly: &Assembly,
                 export: &HostExport,
                 vm_view: &View,
                 dns: Option<IpAddr>|
     -> Result<(Root, Mapper, SourceRoot), Box<dyn Error>> {
        let mut root = root::assemble(assembly)?;
        let source = root.mount_export(&export.tag, export.mode.into())?;
        root.mount_runtime(Some(Path::new("/run/nvx/time")))?;
        let mut target = TargetRoot::open(root.path())?;
        target.add_mount(&root.path().join("run"))?;
        let mapper = Mapper::new(target, Reserved::container_root());
        view::apply(&mut root, vm_view, dns)?;
        // Start the helpers and open the descriptors that later work needs before the switch.
        root.switch()?;
        Ok((root, mapper, source))
    };
    // A restore, as `boot::Restored::apply` builds it from the accepted record.
    let _restored = |record: &ActivationRecord,
                     template: &RootTemplate,
                     deadline: Option<Instant>|
     -> Result<(Applied, Mapper, Option<SourceRoot>), Box<dyn Error>> {
        let mut applied = activation::apply(record, template, deadline)?;
        applied
            .root_mut()
            .mount_runtime(Some(Path::new("/run/nvx/time")))?;
        let mut target = TargetRoot::open(applied.root().path())?;
        target.add_mount(&applied.root().path().join("run"))?;
        let mapper = Mapper::new(target, Reserved::container_root());
        let source = applied.take_export();
        // Start the announcer here when `applied.publication()` publishes the listeners.
        applied.root_mut().switch()?;
        Ok((applied, mapper, source))
    };
}

#[test]
fn restored_sessions_keep_the_deadline_of_the_actual_repair_acknowledgement() {
    let repaired = crate::guest::snapshot::repair::tests::repaired_fixture();
    let acknowledged = repaired.acknowledged_at().unwrap();
    let mut restored = Restored::new(&repaired, acknowledged + Duration::from_secs(12));
    let deadline = acknowledged + Duration::from_secs(30);
    assert_eq!(restored.activation.deadline(), Some(deadline));
    restored
        .reset(acknowledged + Duration::from_secs(13))
        .unwrap();
    restored
        .reset(acknowledged + Duration::from_secs(20))
        .unwrap();
    assert_eq!(restored.activation.deadline(), Some(deadline));
    let (_, payload) = super::super::Capture::activation_fixture();
    assert!(matches!(
        restored
            .accept(&payload, acknowledged + Duration::from_secs(21))
            .unwrap()
            .0,
        ActivationAction::Apply(_)
    ));
    restored
        .reset(acknowledged + Duration::from_secs(29))
        .unwrap();
    assert!(matches!(
        restored
            .accept(&payload, acknowledged + Duration::from_secs(29))
            .unwrap()
            .0,
        ActivationAction::Join
    ));
    assert_eq!(restored.activation.deadline(), Some(deadline));
    assert!(
        restored
            .reset(deadline)
            .unwrap_err()
            .to_string()
            .contains("deadline")
    );
    assert_eq!(restored.activation.state(), ActivationState::Failed);
}

#[test]
fn restored_resource_steps_finish_before_completion_and_ready() {
    use crate::activation::{Network, Publication, ReadyActivation};
    use std::cell::RefCell;
    let (mut capture, mut payload) = super::super::Capture::activation_fixture();
    let (record, _) = protocol::decode_activate_payload(&payload).unwrap();
    let mut configuration = record.configuration().clone();
    configuration.network = Some(Network {
        dns: Some("10.88.0.53".parse().unwrap()),
        publication: Publication::Published,
        proxy: Some("http://10.88.0.1:3128".into()),
    });
    let record =
        activation::ActivationRecord::new(record.restore().copied(), configuration).unwrap();
    payload.truncate(9);
    payload.extend(record.encode());
    let restored = capture.restored.as_mut().unwrap();
    let (ActivationAction::Apply(accepted), _) = restored.accept(&payload, Instant::now()).unwrap()
    else {
        panic!("one accepted activation");
    };
    let calls = RefCell::new(Vec::new());
    let (retained, ()) = restore_in_order(
        None,
        || {
            calls.borrow_mut().push("apply");
            Ok(accepted.configuration().clone())
        },
        |space| {
            calls.borrow_mut().push("mount-runtime");
            assert_eq!(
                space.network.as_ref().unwrap().dns,
                Some("10.88.0.53".parse().unwrap())
            );
            assert_eq!(
                space.upper,
                activation::Upper::Tmpfs {
                    max_memory_percent: 63
                }
            );
            Ok(())
        },
        |space| {
            assert_eq!(
                space.network.as_ref().unwrap().publication,
                Publication::Published
            );
            calls.borrow_mut().push("owned-announcer");
            Ok(())
        },
        |_| {
            calls.borrow_mut().push("switch");
            Ok(())
        },
        |_| panic!("a successful switched root is retained"),
    )
    .unwrap();
    assert_eq!(retained.network, accepted.configuration().network);
    let ready = restored
        .activation
        .complete(Instant::now(), Ok(()))
        .unwrap();
    calls.borrow_mut().push("complete");
    assert_eq!(ready, ReadyActivation::of(&accepted));
    calls.borrow_mut().push("ready");
    assert_eq!(
        *calls.borrow(),
        [
            "apply",
            "mount-runtime",
            "owned-announcer",
            "switch",
            "complete",
            "ready"
        ]
    );
    assert!(matches!(
        restored.accept(&payload, Instant::now()).unwrap().0,
        ActivationAction::Ready(_)
    ));
}

#[test]
fn restored_setup_drops_prepared_handles_before_cleanup_and_preserves_both_errors() {
    use std::cell::RefCell;
    struct Handle<'a>(&'a RefCell<Vec<&'static str>>);
    impl Drop for Handle<'_> {
        fn drop(&mut self) {
            self.0.borrow_mut().push("close-mapper");
        }
    }
    let calls = RefCell::new(Vec::new());
    let error = restore_in_order(
        None,
        || {
            calls.borrow_mut().push("apply");
            Ok(())
        },
        |_| {
            calls.borrow_mut().push("mount-runtime");
            Ok(Handle(&calls))
        },
        |_| {
            calls.borrow_mut().push("announcer-failed");
            Err(Error::io("announcer failed"))
        },
        |_| panic!("failed setup cannot switch"),
        |_| {
            calls.borrow_mut().push("teardown");
            Err(Error::io("teardown failed"))
        },
    )
    .err()
    .unwrap();
    assert_eq!(
        *calls.borrow(),
        [
            "apply",
            "mount-runtime",
            "announcer-failed",
            "close-mapper",
            "teardown"
        ]
    );
    assert!(error.to_string().contains("announcer failed"));
    assert!(error.to_string().contains("teardown failed"));
    assert!(
        std::error::Error::source(&error)
            .unwrap()
            .to_string()
            .contains("announcer failed")
    );
}

#[test]
fn an_expired_activation_cannot_begin_or_advance_to_another_space_step() {
    assert!(
        restore_in_order::<(), ()>(
            Some(Instant::now()),
            || panic!("an expired activation cannot apply"),
            |_| panic!("cannot mount"),
            |_| panic!("cannot announce"),
            |_| panic!("cannot switch"),
            |_| panic!("nothing was applied"),
        )
        .is_err()
    );
    let deadline = Instant::now() + Duration::from_secs(1);
    let applied = std::cell::Cell::new(false);
    let cleaned = std::cell::Cell::new(false);
    let error = restore_in_order::<(), ()>(
        Some(deadline),
        || {
            applied.set(true);
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
            Ok(())
        },
        |_| panic!("an application that used its deadline cannot mount runtime"),
        |_| panic!("cannot announce"),
        |_| panic!("cannot switch"),
        |_| {
            cleaned.set(true);
            Ok(())
        },
    )
    .unwrap_err();
    assert_eq!(cleaned.get(), applied.get());
    assert!(error.to_string().contains("deadline"));
}

fn tokens(extra: &str) -> Tokens {
    Tokens::parse(format!("{ROOT} {VIEW} {CLI} {extra}").as_bytes()).unwrap()
}

#[test]
fn cold_boot_reads_the_actual_root_view_export_maps_and_cli_contracts() {
    let tokens = tokens(&format!(
        "nvx_ram_upper_percent=73 nvx_overlay_mode=untrusted-lowers \
         nvx_workload_account=create nvx_maps=2 virtnet_dns=10.0.0.1 {EXPORT}"
    ));
    assert_eq!(
        tokens.assembly.upper,
        root::Upper::Tmpfs {
            max_memory_percent: Some(73)
        }
    );
    assert_eq!(tokens.assembly.overlay, root::OverlayMode::UntrustedLowers);
    assert_eq!(tokens.assembly.distro.mmio, DISTRO_MMIO_BASE);
    assert_eq!(tokens.view.hostname, b"box");
    assert_eq!(tokens.view.machine_id, MachineIdMode::Generated);
    assert_eq!(tokens.dns, Some("10.0.0.1".parse().unwrap()));
    assert_eq!(tokens.table.expected(), 2);
    assert!(!tokens.table.is_complete());
    assert_eq!(tokens.identity, (1000, 1001));
    assert!(tokens.create_account);
    assert_eq!(tokens.control, Path::new("/dev/hvc2"));
    assert!(
        matches!(tokens.filesystem, Filesystem::Export(HostExport { ref tag, .. }) if tag == "hostfs")
    );
}

#[test]
fn gpt_scratch_settings_and_decoded_non_utf8_hostname_reach_the_assembly() {
    let command = format!(
        "nvx_layer=distro,0xd0003000,gpt:01234567-89ab-cdef-0123-456789abcdef \
         nvx_scratch=0xd0006000,ext4 nvx_scratch_journal=forbidden \
         nvx_first_partition_bound=4096 nvx_overlay_mode=trusted-lowers \
         nvx_hostname=ff2e41 nvx_machine_id=image {CLI}"
    );
    let tokens = Tokens::parse(command.as_bytes()).unwrap();
    assert_eq!(tokens.view.hostname, b"\xff.A");
    assert_eq!(tokens.view.machine_id, MachineIdMode::Image);
    assert_eq!(
        tokens.assembly.upper,
        root::Upper::Scratch {
            device: Device {
                mmio: SCRATCH_MMIO_BASE,
                sectors: None
            },
            journal: root::Journal::Forbidden,
        }
    );
    match tokens.assembly.image {
        root::Image::Gpt {
            expected,
            first_partition_bound,
        } => {
            assert_eq!(first_partition_bound, 4096);
            assert_eq!(
                expected,
                Expected::new().disk_guid("01234567-89ab-cdef-0123-456789abcdef".parse().unwrap())
            );
        }
        other => panic!("not the requested GPT image: {other:?}"),
    }
}

#[test]
fn a_cold_boot_needs_a_complete_layered_contract_and_unambiguous_cli_defaults() {
    for command in [
        format!("{VIEW} {CLI}"),
        format!("{ROOT} {CLI}"),
        format!("{ROOT} {VIEW} nvx_control_tty=hvc2"),
        format!("{ROOT} {VIEW} nvx_identity=1000:1001"),
        format!("{ROOT} {VIEW} {CLI} nvx_maps=1"),
        format!("{ROOT} {VIEW} {CLI} nvx_identity=1000:1001"),
        format!("{ROOT} {VIEW} {CLI} nvx_identity"),
        format!("{ROOT} {VIEW} {CLI} nvx_control_tty"),
        format!("{ROOT} {VIEW} {CLI} nvx_workload_account=create nvx_workload_account=create"),
        format!("{ROOT} {VIEW} nvx_control_tty=../hvc2 nvx_identity=1000:1001"),
        format!("{ROOT} {VIEW} nvx_control_tty=hvc2 nvx_identity=0:1001"),
        format!("{ROOT} {VIEW} nvx_control_tty=hvc2 nvx_identity=1000:4294967295"),
        format!("{ROOT} {VIEW} nvx_control_tty=hvc2 nvx_identity=1000:1001:1002"),
        format!("{ROOT} {VIEW} {CLI} nvx_ram_upper_percent=91"),
    ] {
        let error = Tokens::parse(command.as_bytes()).err().expect(&command);
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{command}: {error}");
    }
}

#[test]
fn live_shares_keep_their_actual_boot_forms_and_never_stand_in_for_an_export() {
    for target in ["/run/app", EXPORT_DIRECTORY] {
        for (mode, access) in [("ro", Access::ReadOnly), ("rw", Access::ReadWrite)] {
            let form = format!("virtfs_dir={target} virtfs_tag=workspace virtfs_mode={mode}");
            let one = tokens(&form);
            assert_eq!(one.assembly.overlay, root::OverlayMode::TrustedLowers);
            assert!(matches!(
                one.filesystem,
                Filesystem::Shared { target: ref actual, access: actual_access, ref children, .. }
                    if actual == Path::new(target) && actual_access == access && children.is_empty()
            ));
            assert!(
                Tokens::parse(format!("{ROOT} {VIEW} {CLI} {form} nvx_maps=1").as_bytes()).is_err()
            );
        }
    }
    let aggregate = tokens(
        "virtfs_dir=/run/nvx/shares virtfs_tag=workspace virtfs_mode=rw virtfs_aggregate=1 \
         nvx_share=0,/run/app,ro nvx_share=1,/etc/app.conf,rw,file",
    );
    match aggregate.filesystem {
        Filesystem::Shared { children, .. } => {
            assert_eq!(children.len(), 2);
            assert_eq!(children[0].target, Path::new("/run/app"));
            assert_eq!(children[0].kind, ShareKind::Directory);
            assert_eq!(children[1].target, Path::new("/etc/app.conf"));
            assert_eq!(children[1].kind, ShareKind::File);
        }
        other => panic!("not the requested shares: {other:?}"),
    }
    for extra in [
        "virtfs_tag=workspace",
        "virtfs_dir=/run/app virtfs_tag=workspace virtfs_mode=ro nvx_maps=1",
        "nvx_share",
        "nvx_share=0,/run/app,ro",
        "virtfs_dir=/run/app virtfs_tag=workspace virtfs_mode=ro virtfs_aggregate=1",
        "virtfs_dir=/run/nvx/shares virtfs_tag=workspace virtfs_mode=rw virtfs_aggregate=2",
        "virtfs_dir=/run/nvx/hostfs/root virtfs_tag=workspace virtfs_mode=ro virtfs_aggregate=2",
        "virtfs_dir=/run/nvx/hostfs/root virtfs_tag=workspace virtfs_aggregate=1",
    ] {
        assert!(
            Tokens::parse(format!("{ROOT} {VIEW} {CLI} {extra}").as_bytes()).is_err(),
            "{extra}"
        );
    }
}

fn account_root(passwd: &[u8], group: &[u8]) -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("etc")).unwrap();
    std::fs::create_dir(directory.path().join("home")).unwrap();
    std::fs::write(directory.path().join("etc/passwd"), passwd).unwrap();
    std::fs::write(directory.path().join("etc/group"), group).unwrap();
    directory
}

#[test]
fn an_existing_account_keeps_its_byte_defaults_without_rewriting_the_databases() {
    for home in ["/home", "/home/", "/home///", "/", "///"] {
        let passwd = [
            b"\xffuser:x:1000:1001::".as_slice(),
            home.as_bytes(),
            b":/bin/sh\n".as_slice(),
        ]
        .concat();
        let group = b"group:x:1001:\n";
        let root = account_root(&passwd, group);
        for create in [false, true] {
            let (user, resolved) = account::resolve(root.path(), (1000, 1001), create).unwrap();
            assert_eq!(user.as_bytes(), b"\xffuser");
            assert_eq!(resolved.as_bytes(), home.as_bytes());
            assert_eq!(
                std::fs::read(root.path().join("etc/passwd")).unwrap(),
                passwd
            );
            assert_eq!(std::fs::read(root.path().join("etc/group")).unwrap(), group);
        }
    }
}

#[test]
fn a_missing_group_is_repaired_without_replacing_a_usable_account() {
    let passwd = b"\xffuser:x:1000:1001::/home:/bin/sh\n";
    let root = account_root(passwd, b"root:x:0:");
    assert!(account::resolve(root.path(), (1000, 1001), false).is_err());
    let (user, home) = account::resolve(root.path(), (1000, 1001), true).unwrap();
    assert_eq!(user.as_bytes(), b"\xffuser");
    assert_eq!(home.as_bytes(), b"/home");
    assert_eq!(
        std::fs::read(root.path().join("etc/passwd")).unwrap(),
        passwd
    );
    assert_eq!(
        std::fs::read(root.path().join("etc/group")).unwrap(),
        b"root:x:0:\nnvx:x:1001:\n"
    );
}

#[test]
fn home_ancestors_resolve_inside_the_real_root_but_the_final_component_is_no_link() {
    for (target, home) in [(".", "/via/home"), ("/", "/via/home")] {
        let passwd = format!("person:x:1000:1001::{home}:/bin/sh\n");
        let root = account_root(passwd.as_bytes(), b"people:x:1001:\n");
        std::os::unix::fs::symlink(target, root.path().join("via")).unwrap();
        for create in [false, true] {
            let (user, resolved) = account::resolve(root.path(), (1000, 1001), create).unwrap();
            assert_eq!(user.as_bytes(), b"person");
            assert_eq!(resolved.as_bytes(), home.as_bytes());
        }
        assert_eq!(
            std::fs::read(root.path().join("etc/passwd")).unwrap(),
            passwd.as_bytes()
        );
    }
    for home in ["/final", "/final/", "/final///"] {
        let passwd = format!("person:x:1000:1001::{home}:/bin/sh\n");
        let group = b"people:x:1001:\n";
        let root = account_root(passwd.as_bytes(), group);
        std::os::unix::fs::symlink("home", root.path().join("final")).unwrap();
        assert!(account::resolve(root.path(), (1000, 1001), false).is_err());
        assert_eq!(
            std::fs::read(root.path().join("etc/passwd")).unwrap(),
            passwd.as_bytes()
        );
        let (user, resolved) = account::resolve(root.path(), (1000, 1001), true).unwrap();
        assert_eq!(
            (user.as_bytes(), resolved.as_bytes()),
            (b"nvx".as_slice(), b"/tmp".as_slice())
        );
        assert_eq!(std::fs::read(root.path().join("etc/group")).unwrap(), group);
    }

    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir(outside.path().join("home")).unwrap();
    let root = account_root(
        b"person:x:1000:1001::/outside/home:/bin/sh\n",
        b"people:x:1001:\n",
    );
    std::os::unix::fs::symlink(outside.path(), root.path().join("outside")).unwrap();
    assert!(account::resolve(root.path(), (1000, 1001), false).is_err());
}

#[test]
fn account_creation_replaces_only_the_requested_unusable_identity_once() {
    let root = account_root(
        b"root:x:0:0::/home:/bin/sh\n# keep:x:1000:0::/:/bin/sh\n\
          old:x:01000:2::/missing:/bin/sh\nbroken:x:1000\n\
          other:x:1000:3::/home:/bin/sh",
        b"root:x:0:",
    );
    assert!(account::resolve(root.path(), (1000, 1001), false).is_err());
    let (user, home) = account::resolve(root.path(), (1000, 1001), true).unwrap();
    assert_eq!(user.as_bytes(), b"nvx");
    assert_eq!(home.as_bytes(), b"/tmp");
    let passwd = std::fs::read(root.path().join("etc/passwd")).unwrap();
    let group = std::fs::read(root.path().join("etc/group")).unwrap();
    assert_eq!(
        passwd,
        b"root:x:0:0::/home:/bin/sh\n# keep:x:1000:0::/:/bin/sh\n\
          nvx:x:1000:1001:NVX workload:/tmp:/sbin/nologin\n"
    );
    assert_eq!(group, b"root:x:0:\nnvx:x:1001:\n");
    account::resolve(root.path(), (1000, 1001), true).unwrap();
    assert_eq!(
        std::fs::read(root.path().join("etc/passwd")).unwrap(),
        passwd
    );
    assert_eq!(std::fs::read(root.path().join("etc/group")).unwrap(), group);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(root.path().join("tmp"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o1777
    );
}

#[test]
fn account_creation_never_replaces_a_link_or_an_unreadable_database() {
    let root = account_root(b"root:x:0:0::/home:/bin/sh\n", b"root:x:0:\n");
    let passwd = root.path().join("etc/passwd");
    let outside = root.path().join("outside");
    std::fs::rename(&passwd, &outside).unwrap();
    std::os::unix::fs::symlink(&outside, &passwd).unwrap();
    assert!(account::resolve(root.path(), (1000, 1001), true).is_err());
    assert_eq!(
        std::fs::read(&outside).unwrap(),
        b"root:x:0:0::/home:/bin/sh\n"
    );
    std::fs::remove_file(&passwd).unwrap();
    std::fs::write(
        &passwd,
        vec![b'x'; accounts::MAX_ACCOUNT_DATABASE_BYTES as usize + 1],
    )
    .unwrap();
    assert!(account::resolve(root.path(), (1000, 1001), true).is_err());
    assert_eq!(
        std::fs::metadata(passwd).unwrap().len(),
        accounts::MAX_ACCOUNT_DATABASE_BYTES + 1
    );
}

/// These tests run only in a dedicated microVM with the candidate's actual root device and
/// filesystem export. They call the production boot sequence, not a simulated root or agent.
fn native_boot() -> Layered {
    assert_eq!(std::env::var("NVX_LAYERED_BOOT_TEST").unwrap(), "1");
    assert!(nix::unistd::geteuid().is_root());
    let command_line = std::fs::read("/proc/cmdline").unwrap();
    let expected = Tokens::parse(&command_line).unwrap().view;
    sys::enter_private_mount_namespace().unwrap();
    nix::sched::unshare(nix::sched::CloneFlags::CLONE_NEWUTS).unwrap();
    let boot = cold_boot(&command_line).unwrap();
    assert_eq!(boot.command_line, command_line);
    assert_eq!(std::env::current_dir().unwrap(), Path::new("/"));
    assert!(
        !boot.root.path().exists(),
        "the old runtime path must be hidden"
    );
    let hostname = std::fs::read("/proc/sys/kernel/hostname").unwrap();
    assert_eq!(hostname.strip_suffix(b"\n").unwrap(), expected.hostname);
    assert_eq!(boot.user.as_bytes(), b"nvx");
    assert_eq!(boot.home.as_bytes(), b"/tmp");
    let tty = nix::sys::termios::tcgetattr(boot.control.borrowed()).unwrap();
    assert!(
        !tty.local_flags.intersects(
            nix::sys::termios::LocalFlags::ECHO | nix::sys::termios::LocalFlags::ICANON
        )
    );
    assert!(!std::fs::read("/run/nvx/time/state").unwrap().is_empty());
    assert_eq!(std::fs::read("/etc/machine-id").unwrap().len(), 33);
    boot
}

#[test]
#[ignore = "needs a dedicated NVX microVM with a real layered image and two announced mappings"]
fn layered_cold_boot_maps_through_handles_opened_before_its_only_switch() {
    let mut boot = native_boot();
    assert_eq!(boot.table.expected(), 2);
    assert!(!boot.table.is_complete());
    assert!(!Path::new(EXPORT_DIRECTORY).exists());
    let maps = ["/workspace", "/run/task-state"].map(|target| protocol::HostMap {
        source: "0".to_owned(),
        target: target.to_owned(),
        read_only: true,
    });
    let parts = protocol::encode_maps_payloads(&maps, 40).unwrap();
    assert_eq!(parts.len(), 2);
    let export = boot
        .export
        .as_ref()
        .expect("the real mounted export is retained");
    assert!(
        boot.table
            .apply(&parts[1], |_| -> std::result::Result<(), (usize, ())> {
                panic!("an out-of-order part must not touch the retained mapper")
            })
            .is_err()
    );
    for (index, part) in parts.iter().enumerate() {
        boot.table
            .apply(part, |batch| {
                boot.mapper
                    .apply(export, batch)
                    .map_err(|error| (error.entry(), error))
            })
            .unwrap();
        assert_eq!(boot.table.is_complete(), index == 1);
    }
    for target in ["/workspace/data", "/run/task-state/data"] {
        assert_eq!(std::fs::read(target).unwrap(), b"sl75b-real-export\n");
    }
    assert!(boot.root.switch().is_err());
    drop(boot);
    assert_eq!(
        std::fs::read("/run/task-state/data").unwrap(),
        b"sl75b-real-export\n"
    );
}

#[test]
#[ignore = "needs a dedicated NVX microVM with a real layered image and live share beneath /run"]
fn layered_cold_boot_mounts_runtime_before_a_live_share_beneath_run() {
    let mut boot = native_boot();
    let file = match filesystem(&boot.command_line).unwrap() {
        Filesystem::Shared {
            target, children, ..
        } if children.is_empty() => target.join("data"),
        other => panic!("not the requested single live share: {other:?}"),
    };
    assert!(boot.export.is_none());
    assert!(boot.table.is_complete());
    assert_eq!(std::fs::read(&file).unwrap(), b"sl75b-real-export\n");
    assert!(boot.root.mount_runtime(None).is_err());
    assert!(boot.root.switch().is_err());
    assert_eq!(std::fs::read(&file).unwrap(), b"sl75b-real-export\n");
    drop(boot);
    assert_eq!(std::fs::read(&file).unwrap(), b"sl75b-real-export\n");
}
