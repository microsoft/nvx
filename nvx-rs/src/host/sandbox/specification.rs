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
use crate::protocol::cmdline::model;

const UUID: &str = "11111111-2222-3333-4444-555555555555";

fn launch(settings: SandboxSettings) -> SandboxLaunch {
    SandboxLaunch::new(
        vec![SandboxLayer::new(LayerRole::Distro, "distro.erofs", UUID)],
        SandboxUpper::scratch("scratch.ext4"),
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
        ("4294967294:4294967294", (u32::MAX - 1, u32::MAX - 1)),
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
        "4294967295:1",
        "1:4294967295",
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
    let launch = SandboxLaunch::new(
        vec![layer],
        SandboxUpper::scratch("scratch.ext4"),
        SandboxSettings::default(),
    )
    .unwrap();
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
        SandboxUpper::scratch("scratch.ext4"),
        SandboxSettings::default(),
    ));
    refused(SandboxLaunch::new(
        vec![launch.distro().clone(); 2],
        SandboxUpper::scratch("scratch.ext4"),
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
                SandboxUpper::scratch("scratch.ext4"),
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
fn accepted_launch_shapes_keep_their_arguments_and_kernel_tokens() {
    let base = "nvx_sandbox=1 \
        nvx_layer=distro,0xd0003000,erofs:11111111-1111-1111-1111-111111111111 \
        nvx_scratch=0xd0006000,ext4 nvx_entrypoint=/bin/sh nvx_hostname=nvx-sandbox";
    let workspace = "0-c52ddf65534b7b46035084358ab7902b";
    let cache = "1-2a93e82e011c66d5c7dbd25da5f2e6c1";
    let file = "0-3ddd35a70a0d7386a5451cd0d5dc3c39";
    let cases = [
        (862, Vec::new(), String::new(), Vec::new()),
        (
            806,
            vec![(
                "/workspace",
                "/host",
                MountAccess::ReadWrite,
                MountKind::Directory,
            )],
            String::new(),
            vec![
                "--mount".to_owned(),
                format!("/workspace,{},rw", host_text("/host")),
            ],
        ),
        (
            664,
            vec![
                (
                    "/workspace",
                    "/host",
                    MountAccess::ReadWrite,
                    MountKind::Directory,
                ),
                (
                    "/srv/cache",
                    "/cache",
                    MountAccess::ReadOnly,
                    MountKind::Directory,
                ),
            ],
            format!(" nvx_share={workspace},/workspace,rw nvx_share={cache},/srv/cache,ro"),
            vec![
                "--mount-aggregate".to_owned(),
                "/run/nvx/shares".to_owned(),
                "--mount-child".to_owned(),
                format!("{workspace},{},rw", host_text("/host")),
                "--mount-child".to_owned(),
                format!("{cache},{},ro", host_text("/cache")),
            ],
        ),
        (
            706,
            vec![(
                "/etc/app/settings.json",
                "/settings.json",
                MountAccess::ReadOnly,
                MountKind::File,
            )],
            format!(" nvx_share={file},/etc/app/settings.json,ro,file"),
            vec![
                "--mount-aggregate".to_owned(),
                "/run/nvx/shares".to_owned(),
                "--mount-child".to_owned(),
                format!("{file},{},ro,file", host_text("/settings.json")),
            ],
        ),
    ];
    for (padding, shares, suffix, tail) in cases {
        let launch = SandboxLaunch::new(
            vec![SandboxLayer::new(
                LayerRole::Distro,
                "layer.erofs",
                "11111111-1111-1111-1111-111111111111",
            )],
            SandboxUpper::scratch("scratch.ext4"),
            SandboxSettings::default(),
        )
        .unwrap()
        .with_export((!shares.is_empty()).then(|| exported(&shares)))
        .unwrap();
        let mut expected = [
            "--machine",
            "microvm",
            "--microvm-sandbox-block",
            "distro:file:layer.erofs,ro",
            "--microvm-sandbox-block",
            "scratch:file:scratch.ext4",
            "--microvm-workload-identity",
            "65534:65534",
        ]
        .map(str::to_owned)
        .to_vec();
        expected.extend(tail);
        assert_eq!(
            launch.plan().one_shot_openvmm_arguments().unwrap(),
            expected
        );
        assert_eq!(
            launch
                .plan()
                .one_shot_kernel_command_line(&"x".repeat(padding))
                .unwrap(),
            format!("{} {base}{suffix}", "x".repeat(padding)),
        );
        refused(
            launch
                .plan()
                .one_shot_kernel_command_line(&"x".repeat(padding + 1)),
        );
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
        memory_max: Some(u64::MAX),
        pids_max: Some(4_194_303),
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
            SandboxUpper::scratch("scratch.ext4"),
            SandboxSettings {
                memory_max: Some(u64::from(memory)),
                pids_max: Some(pids),
                ..SandboxSettings::default()
            },
        ));
    }
    for entrypoint in ["bin/sh", "/bin/a b", "/bin/a\"b"] {
        refused(SandboxLaunch::new(
            vec![SandboxLayer::new(LayerRole::Distro, "distro.erofs", UUID)],
            SandboxUpper::scratch("scratch.ext4"),
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
    let other_drive = if drive.eq_ignore_ascii_case("Z") {
        "Y"
    } else {
        "Z"
    };
    let other_relative = format!("{other_drive}:share");
    let drive_only = format!("{other_drive}:");
    for (value, expected) in [
        (format!("{drive}:share"), cwd.join("share")),
        (
            other_relative.clone(),
            std::path::absolute(&other_relative).unwrap(),
        ),
        (
            drive_only.clone(),
            std::path::absolute(&drive_only).unwrap(),
        ),
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

#[cfg(windows)]
#[test]
fn windows_drive_relative_policy_paths_become_absolute() {
    let cwd = std::env::current_dir().unwrap();
    let drive = cwd.to_str().unwrap().split(':').next().unwrap();
    let other_drive = if drive.eq_ignore_ascii_case("Z") {
        "Y"
    } else {
        "Z"
    };
    let relative = format!("{other_drive}:secret");
    let mount = SandboxMount::new(
        "/workspace",
        cwd.to_str().unwrap(),
        MountAccess::ReadOnly,
        MountPolicy {
            denied: vec![relative.clone()],
            ..MountPolicy::default()
        },
        MountOwner::Vmm,
        MountKind::Directory,
    )
    .unwrap();
    let paths = mount.absolute_policy_paths().unwrap();
    assert_eq!(
        Path::new(&paths[0].1[0]),
        std::path::absolute(relative).unwrap()
    );
    assert!(Path::new(&paths[0].1[0]).is_absolute());
}

#[test]
fn kernel_token_values_refuse_separators() {
    for separator in [
        '\0', '\t', '\n', '\u{b}', '\u{c}', '\r', ' ', '"', '\u{a0}', '\u{120}', '\u{3000}',
        '\u{1c}',
    ] {
        for settings in [
            SandboxSettings {
                entrypoint: format!("/bin/a{separator}b"),
                ..SandboxSettings::default()
            },
            SandboxSettings {
                args: vec![format!("a{separator}b")],
                ..SandboxSettings::default()
            },
        ] {
            refused(SandboxLaunch::new(
                vec![SandboxLayer::new(LayerRole::Distro, "distro.erofs", UUID)],
                SandboxUpper::scratch("scratch.ext4"),
                settings,
            ));
        }
        refused(validate_mount_target(&format!("/workspace/a{separator}b")));
    }
}

#[test]
fn caller_parameters_follow_linux_and_guest_rules() {
    let launch = launch(SandboxSettings {
        entrypoint: "/bin/workload".to_owned(),
        memory_max: Some(268_435_456),
        pids_max: Some(64),
        ..SandboxSettings::default()
    });
    let adapter = SandboxPlan::adapter(
        SandboxDistro::new("distro.erofs", RootImage::erofs(Uuid::parse(UUID))),
        SandboxUpper::RAM,
    );
    for user in [
        r#""nvx_layer=other""#,
        r#""virtfs_dir=/other""#,
        "nvx-layer=other",
        "virtfs-dir=/other",
        "quiet\u{120}nvx_layer=other",
        "quiet\u{a0}virtfs_dir=/other",
        "\u{2003}nvx_layer=other",
        "\u{2003}virtfs_dir=/other",
        r#"a="nvx_layer=other virtfs_dir=/other""#,
        r#"extra="ignored virtfs_dir=/other ignored""#,
        "--",
        "quiet -- ignored",
        "value=\"unterminated",
    ] {
        refused(launch.plan().kernel_command_line(user));
        refused(launch.plan().one_shot_kernel_command_line(user));
        refused(adapter.kernel_command_line(user));
    }
    for user in [
        "\u{2003}nvx_entrypoint=/bin/other",
        r#"extra="ignored nvx_entrypoint=/bin/other ignored""#,
        r#"extra="ignored nvx_memory_max=max ignored""#,
        r#"extra="ignored nvx_pids_max=0 ignored""#,
    ] {
        refused(launch.plan().kernel_command_line(user));
        refused(launch.plan().one_shot_kernel_command_line(user));
    }
    for user in [
        r#"a="b c" d"#,
        r#"a="ordinary=value other=value""#,
        r#""ordinary=value with spaces""#,
    ] {
        for command_line in [
            launch.plan().kernel_command_line(user).unwrap(),
            launch.plan().one_shot_kernel_command_line(user).unwrap(),
            adapter.kernel_command_line(user).unwrap(),
        ] {
            assert!(command_line.starts_with(&format!("{user} ")));
        }
    }
    assert!(adapter.kernel_command_line("nvx_custom=adapter").is_ok());
}

#[test]
fn the_reproduction_of_microsoft_nvx_723_is_refused() {
    // `à`, U+00E0, is C3 A0 in UTF-8, and the kernel takes the byte 0xa0 for whitespace; it also
    // drops the quote that starts a parameter.
    let launch = launch(SandboxSettings::default());
    for user in [
        "quiet\u{e0}nvx_entrypoint=/bin/other",
        r#""nvx_entrypoint=/bin/other""#,
    ] {
        refused(launch.plan().kernel_command_line(user));
        refused(launch.plan().one_shot_kernel_command_line(user));
    }
    refused(SandboxLaunch::new(
        vec![SandboxLayer::new(LayerRole::Distro, "distro.erofs", UUID)],
        SandboxUpper::scratch("scratch.ext4"),
        SandboxSettings {
            args: vec!["x\u{e0}tsc=unstable".to_owned()],
            ..SandboxSettings::default()
        },
    ));
    for target in ["/data/\u{e0}x", r#"/data/a"b"#] {
        refused(validate_mount_target(target));
    }
    // The kernel honors quoted values, so a caller's command line may hold them.
    for user in [r#"a="b c" d"#, r#"quiet a="b c""#] {
        let command_line = launch.plan().kernel_command_line(user).unwrap();
        assert!(
            command_line.starts_with(&format!("{user} ")),
            "{command_line}"
        );
    }
}

/// Returns a strategy for a value that becomes part of a kernel token, such as an argument:
/// mostly characters that it may hold, and now and then a NUL, a quote, a character that the
/// kernel reads as whitespace, whole or in part, or Unicode whitespace at which it splits nothing.
/// U+00A0, U+00E0, U+0120, and U+2020 end with the byte 0xa0 in UTF-8, and U+0800 holds it
/// before its last byte.
fn token_value() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            12 => prop::char::range('a', 'z'),
            3 => prop::sample::select(vec!['/', '-', '_', '.', '=', '\u{e9}', '\u{4e2d}']),
            2 => prop::sample::select(vec![
                '\0', '\t', '\n', '\u{b}', '\u{c}', '\r', ' ', '"', '\u{a0}', '\u{e0}',
                '\u{120}', '\u{2020}', '\u{800}', '\u{2003}',
            ]),
        ],
        1..12,
    )
    .prop_map(String::from_iter)
}

proptest! {
    #[test]
    fn non_root_workload_identities_round_trip(uid in 1..u32::MAX, gid in 1..u32::MAX) {
        prop_assert_eq!(parse_workload_identity(&format!("{uid}:{gid}")).unwrap(), (uid, gid));
    }

    #[test]
    fn canonical_layer_uuids_reach_the_guest(bytes in any::<[u8; 16]>()) {
        let uuid = Uuid::from_bytes(bytes).to_string();
        let layer = SandboxLayer::parse(&format!("distro,distro.erofs,{uuid}")).unwrap();
        prop_assert_eq!(layer.uuid(), &uuid);
        let launch = SandboxLaunch::new(vec![layer], SandboxUpper::scratch("scratch.ext4"), SandboxSettings::default()).unwrap();
        let command_line = launch.plan().kernel_command_line("").unwrap();
        let expected = format!("nvx_layer=distro,0xd0003000,erofs:{uuid}");
        prop_assert!(command_line.split_ascii_whitespace().any(|token| token == expected));
    }

    #[test]
    fn reserved_mount_targets_are_refused(suffix in "[a-z][a-z0-9-]{0,20}") {
        for tree in [
            "/proc",
            "/sys",
            "/dev",
            "/.nvx-agent",
            "/etc/machine-id",
            "/etc/resolv.conf",
            "/run/nvx/time",
        ] {
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
        separator in prop::sample::select(vec![" ", "\t", "\n", "\r", "\u{b}", "\u{c}", "\u{a0}", "\u{120}"]),
        quoted in any::<bool>(),
        nested in any::<bool>(),
        leading_unicode in any::<bool>(),
    ) {
        let launch = launch(SandboxSettings::default());
        for prefix in ["nvx_", "virtfs_", "nvx-", "virtfs-"] {
            let token = format!("{prefix}{name}={value}");
            let token = if nested {
                format!("extra=\"ignored {token} ignored\"")
            } else if quoted {
                format!("\"{token}\"")
            } else {
                token
            };
            let user = if leading_unicode {
                format!("\u{2003}{token}")
            } else {
                format!("quiet{separator}{token}")
            };
            refused(launch.plan().kernel_command_line(&user));
            refused(launch.plan().one_shot_kernel_command_line(&user));
        }
    }

    /// A value that becomes one kernel token, an entrypoint, an argument, or a share's target, is
    /// one parameter for the kernel: a value in which the kernel would end a parameter, with
    /// whitespace, a quote, or a NUL, is refused, and the kernel reads every token of the command
    /// lines of a launch whose values are accepted as one parameter, whole.
    #[test]
    fn accepted_values_stay_one_kernel_parameter(
        entrypoint in token_value(),
        argument in token_value(),
        first in token_value(),
        second in token_value(),
    ) {
        let splits = |value: &str| {
            value.bytes().any(|byte| matches!(byte, b'\0' | b'"') || model::isspace(byte))
        };
        let entrypoint = format!("/{entrypoint}");
        let targets = [format!("/share-0/{first}"), format!("/share-1/{second}")];
        let launch = SandboxLaunch::new(
            vec![SandboxLayer::new(LayerRole::Distro, "distro.erofs", UUID)],
            SandboxUpper::scratch("scratch.ext4"),
            SandboxSettings {
                entrypoint: entrypoint.clone(),
                args: vec![argument.clone()],
                ..SandboxSettings::default()
            },
        );
        if splits(&entrypoint) || splits(&argument) {
            prop_assert!(launch.is_err(), "{:?} {:?}", entrypoint, argument);
        }
        for target in &targets {
            if splits(target) {
                prop_assert!(validate_mount_target(target).is_err(), "{:?}", target);
            }
        }
        let accepted = targets.iter().all(|target| validate_mount_target(target).is_ok());
        let (Ok(launch), true) = (launch, accepted) else {
            return Ok(());
        };
        let shares: Vec<_> = targets
            .iter()
            .zip(["/host-0", "/host-1"])
            .map(|(target, root)| (target.as_str(), root, MountAccess::ReadWrite, MountKind::Directory))
            .collect();
        let launch = launch.with_export(Some(exported(&shares))).unwrap();
        for command_line in [
            launch.plan().kernel_command_line("quiet").unwrap(),
            launch.plan().one_shot_kernel_command_line("quiet").unwrap(),
        ] {
            let reading = model::read(command_line.as_bytes());
            prop_assert_eq!(&reading.rest, &None);
            let tokens: Vec<&[u8]> = command_line.split(' ').map(str::as_bytes).collect();
            prop_assert_eq!(reading.lines(), tokens);
        }
    }
}
