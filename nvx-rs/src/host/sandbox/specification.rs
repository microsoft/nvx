//! Specifications of the documented launch options and the contracts with OpenVMM and the guest.
//! These tests deliberately do not specify Python's input extensions or error formatting.

use std::fs;
use std::path::PathBuf;

use proptest::prelude::*;
use sha2::{Digest, Sha256};

use super::mount::{aggregate_child_name, parse_mount_value, validate_mount_target};
use super::tests::{exported, host_text};
use super::*;
use crate::ErrorKind;

const UUID: &str = "11111111-2222-3333-4444-555555555555";

fn launch(settings: SandboxSettings) -> SandboxLaunch {
    SandboxLaunch::new(
        vec![SandboxLayer::new(LayerRole::Distro, "distro.erofs", UUID)],
        "scratch.ext4",
        settings,
    )
    .unwrap()
}

fn refused<T: std::fmt::Debug>(result: Result<T>) {
    assert_eq!(result.unwrap_err().kind(), ErrorKind::Unsupported);
}

#[test]
fn workload_identity_grammar() {
    for (value, expected) in [
        ("1:1", (1, 1)),
        ("1000:1001", (1000, 1001)),
        ("4294967295:4294967295", (u32::MAX, u32::MAX)),
    ] {
        assert_eq!(parse_workload_identity(value).unwrap(), expected);
    }
    for value in [
        "",
        "1",
        ":1",
        "1:",
        "1:2:3",
        "0:1",
        "1:0",
        "-1:1",
        "1:-1",
        "4294967296:1",
        "1:4294967296",
        "one:1",
        "1:1.0",
        "1\0:2",
    ] {
        refused(parse_workload_identity(value));
    }
}

#[test]
fn layer_values_are_role_path_uuid() {
    assert_eq!(LAYER_ROLES, [LayerRole::Distro]);
    let layer = SandboxLayer::parse(&format!("distro,distro.erofs,{UUID}")).unwrap();
    assert_eq!(layer.role(), LayerRole::Distro);
    assert_eq!(layer.path(), Path::new("distro.erofs"));
    assert_eq!(layer.format(), DistroFormat::Erofs);
    assert_eq!(layer.uuid(), UUID);
    let launch =
        SandboxLaunch::new(vec![layer], "scratch.ext4", SandboxSettings::default()).unwrap();
    assert!(
        launch
            .plan()
            .kernel_command_line("")
            .unwrap()
            .split_ascii_whitespace()
            .any(|token| token == format!("nvx_layer=distro,0xd0003000,erofs:{UUID}"))
    );
    for role in ["runtime", "custom", "Distro", "", "unknown"] {
        refused(SandboxLayer::parse(&format!("{role},distro.erofs,{UUID}")));
    }
    for value in [
        "distro",
        "distro,distro.erofs",
        "distro,distro.erofs,not-a-uuid",
        "distro,distro.erofs,",
        "distro,,11111111-2222-3333-4444-555555555555",
        "distro,distro.erofs,11111111-2222-3333-4444-555555555555,extra",
    ] {
        refused(SandboxLayer::parse(value));
    }
    refused(SandboxLaunch::new(
        Vec::new(),
        "scratch.ext4",
        SandboxSettings::default(),
    ));
    refused(SandboxLaunch::new(
        vec![launch.distro().clone(); 2],
        "scratch.ext4",
        SandboxSettings::default(),
    ));
}

#[test]
fn mount_value_grammar() {
    for (value, target, host, access) in [
        (
            "/workspace,host",
            "/workspace",
            "host",
            MountAccess::ReadOnly,
        ),
        (
            "/workspace,host,ro",
            "/workspace",
            "host",
            MountAccess::ReadOnly,
        ),
        (
            "/workspace,host,rw",
            "/workspace",
            "host",
            MountAccess::ReadWrite,
        ),
        (
            "/workspace,host,with,commas,rw",
            "/workspace",
            "host,with,commas",
            MountAccess::ReadWrite,
        ),
        (
            "/workspace,host,ro,ro",
            "/workspace",
            "host,ro",
            MountAccess::ReadOnly,
        ),
    ] {
        assert_eq!(
            parse_mount_value(value).unwrap(),
            (target.to_owned(), host.to_owned(), access),
        );
    }
    for value in [
        "",
        "/workspace",
        "/workspace,",
        "/workspace,,ro",
        "/workspace,,rw",
        "/workspace,host,",
        "/workspace,host,RW",
        "/workspace,host,with,commas",
    ] {
        refused(parse_mount_value(value));
    }
    for value in [",host", "relative,host", "/,host"] {
        refused(SandboxMount::parse(
            value,
            MountPolicy::default(),
            MountOwner::Vmm,
            MountKind::Directory,
        ));
    }
}

#[test]
fn mount_targets_are_canonical_and_byte_bounded() {
    for target in [
        "",
        "/",
        "relative",
        "//workspace",
        "/workspace/",
        "/a//b",
        "/a/./b",
        "/a/../b",
        "/a b",
        "/a\tb",
        "/a\nb",
        "/a\\b",
        "/a=b",
        "/a,b",
        "/a\0b",
    ] {
        refused(validate_mount_target(target));
    }
    validate_mount_target(&format!("/{}", "x".repeat(4095))).unwrap();
    refused(validate_mount_target(&format!("/{}", "x".repeat(4096))));
    let multibyte = format!("/{}", "\u{e9}".repeat(2047));
    validate_mount_target(&multibyte).unwrap();
    refused(validate_mount_target(&format!("{multibyte}\u{e9}")));
}

#[test]
fn aggregate_child_name_known_answer() {
    assert_eq!(
        aggregate_child_name(0, "/workspace"),
        "0-c52ddf65534b7b46035084358ab7902b",
    );
    assert_eq!(
        aggregate_child_name(17, "/workspace"),
        "17-c52ddf65534b7b46035084358ab7902b",
    );
}

#[test]
fn launch_arguments_follow_the_openvmm_contract() {
    for format in [DistroFormat::Erofs, DistroFormat::Gpt] {
        for identity in [(1, 2), (65534, 65534), (u32::MAX, u32::MAX)] {
            let layer = match format {
                DistroFormat::Erofs => SandboxLayer::new(LayerRole::Distro, "distro.img", UUID),
                DistroFormat::Gpt => SandboxLayer::gpt(LayerRole::Distro, "distro.img", UUID),
            };
            let launch = SandboxLaunch::new(
                vec![layer],
                "scratch.ext4",
                SandboxSettings {
                    workload_identity: identity,
                    ..SandboxSettings::default()
                },
            )
            .unwrap();
            let expected = [
                "--machine".to_owned(),
                "microvm".to_owned(),
                "--microvm-sandbox-block".to_owned(),
                "distro:file:distro.img,ro".to_owned(),
                "--microvm-sandbox-block".to_owned(),
                "scratch:file:scratch.ext4".to_owned(),
                "--microvm-workload-identity".to_owned(),
                format!("{}:{}", identity.0, identity.1),
            ];
            assert_eq!(launch.plan().openvmm_arguments().unwrap(), expected);
            assert_eq!(
                launch.plan().one_shot_openvmm_arguments().unwrap(),
                expected
            );
            assert_eq!(
                launch.plan().kernel_command_line("quiet").unwrap(),
                format!(
                    "quiet nvx_sandbox=1 nvx_layer=distro,0xd0003000,{format}:{UUID} \
                     nvx_scratch=0xd0006000,ext4 nvx_entrypoint=/bin/sh nvx_hostname=nvx-sandbox"
                ),
            );
        }
    }
}

#[test]
fn shares_follow_the_openvmm_and_guest_contracts() {
    for (access, mode) in [
        (MountAccess::ReadOnly, "ro"),
        (MountAccess::ReadWrite, "rw"),
    ] {
        for kind in [MountKind::Directory, MountKind::File] {
            let export = exported(&[("/workspace", "/host/share", access, kind)]);
            let launch = launch(SandboxSettings::default())
                .with_export(Some(export))
                .unwrap();
            let arguments = launch.plan().one_shot_openvmm_arguments().unwrap();
            let command_line = launch.plan().one_shot_kernel_command_line("").unwrap();
            if kind == MountKind::Directory {
                assert_eq!(
                    &arguments[8..],
                    [
                        "--mount",
                        &format!("/workspace,{},{}", host_text("/host/share"), mode)
                    ],
                );
                assert!(!command_line.contains("nvx_share="));
            } else {
                let name = "0-c52ddf65534b7b46035084358ab7902b";
                assert_eq!(
                    &arguments[8..],
                    [
                        "--mount-aggregate",
                        "/run/nvx/shares",
                        "--mount-child",
                        &format!("{name},{},{mode},file", host_text("/host/share")),
                    ],
                );
                assert!(
                    command_line.ends_with(&format!(" nvx_share={name},/workspace,{mode},file"))
                );
            }
            assert!(
                launch
                    .plan()
                    .kernel_command_line("")
                    .unwrap()
                    .ends_with(" nvx_maps=1")
            );
        }
    }
}

#[test]
fn launch_settings_follow_the_guest_contract() {
    let settings = SandboxSettings {
        entrypoint: "/usr/bin/workload".to_owned(),
        args: vec!["first".to_owned(), "--second=value".to_owned()],
        hostname: "workload-1".to_owned(),
        workload_identity: (1000, 1001),
        memory_max: Some(Int::from(u64::MAX)),
        pids_max: Some(Int::from(4_194_303_u32)),
    };
    assert_eq!(
        launch(settings).plan().kernel_command_line("").unwrap(),
        format!(
            "nvx_sandbox=1 nvx_layer=distro,0xd0003000,erofs:{UUID} \
             nvx_scratch=0xd0006000,ext4 nvx_entrypoint=/usr/bin/workload \
             nvx_hostname=workload-1 nvx_arg=first nvx_arg=--second=value \
             nvx_memory_max=18446744073709551615 nvx_pids_max=4194304"
        ),
    );
    for (memory, pids) in [(0_u32, 1_u32), (1, 0), (1, 4_194_304)] {
        refused(SandboxLaunch::new(
            vec![SandboxLayer::new(LayerRole::Distro, "distro.erofs", UUID)],
            "scratch.ext4",
            SandboxSettings {
                memory_max: Some(Int::from(memory)),
                pids_max: Some(Int::from(pids)),
                ..SandboxSettings::default()
            },
        ));
    }
    for entrypoint in ["bin/sh", "/bin/a b", "/bin/a\"b"] {
        refused(SandboxLaunch::new(
            vec![SandboxLayer::new(LayerRole::Distro, "distro.erofs", UUID)],
            "scratch.ext4",
            SandboxSettings {
                entrypoint: entrypoint.to_owned(),
                ..SandboxSettings::default()
            },
        ));
    }
}

#[cfg(windows)]
#[test]
fn windows_drive_relative_and_unc_paths() {
    let cwd = std::env::current_dir().unwrap();
    let drive = cwd.to_str().unwrap().split(':').next().unwrap();
    for (value, expected) in [
        (format!("{drive}:share"), cwd.join("share")),
        (
            r"\share".to_owned(),
            PathBuf::from(format!(r"{drive}:\share")),
        ),
        (
            r"\\server\share\dir".to_owned(),
            PathBuf::from(r"\\server\share\dir"),
        ),
        (
            r"\\server\share".to_owned(),
            PathBuf::from(r"\\server\share\"),
        ),
    ] {
        let mount = SandboxMount::new(
            "/workspace",
            &value,
            MountAccess::ReadOnly,
            MountPolicy::default(),
            MountOwner::Vmm,
            MountKind::Directory,
        )
        .unwrap()
        .absolute()
        .unwrap();
        assert_eq!(mount.host_path(), expected);
        assert!(mount.host_path().is_absolute());
    }
}

proptest! {
    #[test]
    fn non_root_workload_identities_round_trip(uid in 1..=u32::MAX, gid in 1..=u32::MAX) {
        prop_assert_eq!(parse_workload_identity(&format!("{uid}:{gid}")).unwrap(), (uid, gid));
    }

    #[test]
    fn canonical_layer_uuids_reach_the_guest(bytes in any::<[u8; 16]>()) {
        let uuid = Uuid::from_bytes(bytes).to_string();
        let layer = SandboxLayer::parse(&format!("distro,distro.erofs,{uuid}")).unwrap();
        prop_assert_eq!(layer.uuid(), &uuid);
        let launch = SandboxLaunch::new(vec![layer], "scratch.ext4", SandboxSettings::default()).unwrap();
        let command_line = launch.plan().kernel_command_line("").unwrap();
        let expected = format!("nvx_layer=distro,0xd0003000,erofs:{uuid}");
        prop_assert!(command_line.split_ascii_whitespace().any(|token| token == expected));
    }

    #[test]
    fn reserved_mount_targets_are_refused(suffix in "[a-z][a-z0-9-]{0,20}") {
        for tree in ["/proc", "/sys", "/dev", "/.nvx-agent", "/etc/machine-id", "/run/nvx/time"] {
            refused(validate_mount_target(tree));
            refused(validate_mount_target(&format!("{tree}/{suffix}")));
            let sibling = format!("{tree}-{suffix}");
            prop_assert!(validate_mount_target(&sibling).is_ok());
        }
        for exact in ["/etc", "/run", "/run/nvx"] {
            refused(validate_mount_target(exact));
            let child = format!("{exact}/app-{suffix}");
            prop_assert!(validate_mount_target(&child).is_ok());
        }
    }

    #[test]
    fn aggregate_child_names_follow_the_restore_contract(
        index in any::<usize>(),
        target in any::<String>(),
    ) {
        let digest = format!("{:x}", Sha256::digest(target.as_bytes()));
        let expected = format!("{index}-{}", &digest[..32]);
        prop_assert_eq!(aggregate_child_name(index, &target), expected);
    }

    #[test]
    fn host_paths_become_absolute(components in prop::collection::vec("dir-[a-z]{1,12}", 1..5)) {
        let cwd = std::env::current_dir().unwrap();
        let directory = tempfile::tempdir_in(&cwd).unwrap();
        let suffix: PathBuf = components.iter().collect();
        let expected = directory.path().join(&suffix);
        fs::create_dir_all(&expected).unwrap();
        let relative = PathBuf::from(directory.path().file_name().unwrap()).join(&suffix);
        let mount = SandboxMount::new(
            "/workspace", relative.to_str().unwrap(), MountAccess::ReadOnly,
            MountPolicy::default(), MountOwner::Vmm, MountKind::Directory,
        ).unwrap();
        let absolute = mount.absolute().unwrap();
        prop_assert_eq!(absolute.host_path(), &expected);
        prop_assert!(absolute.host_path().is_absolute());
        prop_assert!(absolute.host_path().is_dir());
        prop_assert_eq!(absolute.absolute().unwrap(), absolute);
    }

    #[test]
    fn kernel_command_line_stays_within_1024_bytes(
        argument in "[a-z0-9]{1,40}",
        user_bytes in 0_usize..1200,
        share_count in 0_usize..4,
        writable in any::<bool>(),
    ) {
        let access = if writable { MountAccess::ReadWrite } else { MountAccess::ReadOnly };
        let targets: Vec<String> = (0..share_count).map(|index| format!("/share-{index}")).collect();
        let roots: Vec<String> = (0..share_count).map(|index| format!("/host-{index}")).collect();
        let shares: Vec<_> = targets.iter().zip(&roots)
            .map(|(target, root)| (target.as_str(), root.as_str(), access, MountKind::Directory)).collect();
        let settings = SandboxSettings { args: vec![argument.clone()], ..SandboxSettings::default() };
        let launch = launch(settings).with_export(
            (!shares.is_empty()).then(|| exported(&shares)),
        ).unwrap();
        let user = "x".repeat(user_bytes);
        for one_shot in [false, true] {
            let render = |user: &str| if one_shot {
                launch.plan().one_shot_kernel_command_line(user)
            } else {
                launch.plan().kernel_command_line(user)
            };
            let mut tokens = vec![
                "nvx_sandbox=1".to_owned(),
                format!("nvx_layer=distro,0xd0003000,erofs:{UUID}"),
                "nvx_scratch=0xd0006000,ext4".to_owned(),
                "nvx_entrypoint=/bin/sh".to_owned(),
                "nvx_hostname=nvx-sandbox".to_owned(),
                format!("nvx_arg={argument}"),
            ];
            if one_shot && share_count > 1 {
                for (index, target) in targets.iter().enumerate() {
                    let digest = format!("{:x}", Sha256::digest(target.as_bytes()));
                    tokens.push(format!("nvx_share={index}-{},{target},{}", &digest[..32], access.as_str()));
                }
            } else if !one_shot && share_count != 0 {
                tokens.push(format!("nvx_maps={share_count}"));
            }
            let base = tokens.join(" ");
            prop_assert_eq!(render("").unwrap(), base.clone());
            let mode = access.as_str();
            let fragment = if share_count == 0 {
                String::new()
            } else if !one_shot {
                " virtfs_dir=/run/nvx/hostfs/root virtfs_tag=microvm virtfs_mode=ro virtfs_aggregate=1".to_owned()
            } else if share_count == 1 {
                format!(" virtfs_dir=/share-0 virtfs_tag=microvm virtfs_mode={mode}")
            } else {
                format!(" virtfs_dir=/run/nvx/shares virtfs_tag=microvm virtfs_mode={mode} virtfs_aggregate=1")
            };
            let bytes = base.len() + user_bytes + usize::from(user_bytes != 0) + fragment.len() + 1;
            match render(&user) {
                Ok(command_line) => {
                    prop_assert!(bytes <= 1024, "{bytes}");
                    let expected = if user.is_empty() { base.clone() } else { format!("{user} {base}") };
                    prop_assert_eq!(command_line, expected);
                }
                Err(error) => {
                    prop_assert!(bytes > 1024, "{bytes}: {error}");
                    prop_assert_eq!(error.kind(), ErrorKind::Unsupported);
                }
            }
            let fitting = 1024 - base.len() - fragment.len() - 2;
            let boundary = "x".repeat(fitting);
            prop_assert_eq!(render(&boundary).unwrap().len() + fragment.len() + 1, 1024);
            refused(render(&format!("{boundary}x")));
        }
    }

    #[test]
    fn reserved_kernel_tokens_are_always_refused(
        name in "[a-z][a-z0-9_]{0,24}",
        value in "[a-z0-9]{0,24}",
        separator in prop::sample::select(vec![" ", "\t", "\n", "\r", "\u{b}", "\u{c}"]),
    ) {
        let launch = launch(SandboxSettings::default());
        for prefix in ["nvx_", "virtfs_"] {
            let user = format!("quiet{separator}{prefix}{name}={value}");
            refused(launch.plan().kernel_command_line(&user));
            refused(launch.plan().one_shot_kernel_command_line(&user));
        }
    }
}
