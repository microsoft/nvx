//! Specifications of the documented options of the sandbox profile: workload identities, layers,
//! and shares.
//! These tests deliberately do not specify Python's input extensions or error formatting.

use std::fs;
use std::path::PathBuf;

use proptest::prelude::*;

#[cfg(feature = "cli")]
use super::distro::DistroFormat;
#[cfg(feature = "cli")]
use super::layer::{LAYER_ROLES, LayerRole, SandboxLayer};
use super::mount::tests::validate_mount_target;
use super::mount::{
    MountAccess, MountKind, MountOwner, MountPolicy, SandboxMount, parse_mount_value,
};
use super::*;
use crate::ErrorKind;
use crate::protocol::cmdline::tests::model;
#[cfg(feature = "cli")]
use crate::uuid::Uuid;

#[cfg(feature = "cli")]
const UUID: &str = "11111111-2222-3333-4444-555555555555";

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

#[cfg(feature = "cli")]
#[test]
fn layer_values_are_role_path_uuid() {
    assert_eq!(LAYER_ROLES, [LayerRole::Distro]);
    let layer = SandboxLayer::parse(&format!("distro,distro.erofs,{UUID}")).unwrap();
    assert_eq!(layer.role(), LayerRole::Distro);
    assert_eq!(layer.path(), Path::new("distro.erofs"));
    assert_eq!(layer.format(), DistroFormat::Erofs);
    assert_eq!(layer.uuid(), UUID);
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
        .absolute_with(&paths::current_dir)
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
    let paths = mount
        .absolute_policy_paths_with(&paths::current_dir)
        .unwrap();
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
        refused(validate_mount_target(&format!("/workspace/a{separator}b")));
    }
}

#[test]
fn the_reproduction_of_microsoft_nvx_723_is_refused() {
    // `à`, U+00E0, is C3 A0 in UTF-8, and the kernel takes the byte 0xa0 for whitespace; it also
    // drops the quote that starts a parameter.
    for target in ["/data/\u{e0}x", r#"/data/a"b"#] {
        refused(validate_mount_target(target));
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

    #[cfg(feature = "cli")]
    #[test]
    fn canonical_layer_uuids_round_trip(bytes in any::<[u8; 16]>()) {
        let uuid = Uuid::from_bytes(bytes).to_string();
        let layer = SandboxLayer::parse(&format!("distro,distro.erofs,{uuid}")).unwrap();
        prop_assert_eq!(layer.uuid(), &uuid);
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
        let absolute = mount.absolute_with(&paths::current_dir).unwrap();
        prop_assert_eq!(absolute.host_path(), &expected);
        prop_assert!(absolute.host_path().is_absolute());
        prop_assert!(absolute.host_path().is_dir());
        prop_assert_eq!(absolute.absolute_with(&paths::current_dir).unwrap(), absolute);
    }

    /// A share's target is one value for the guest: a target in which the kernel would end a
    /// parameter, with whitespace, a quote, or a NUL, is refused.
    #[test]
    fn accepted_values_stay_one_kernel_parameter(first in token_value(), second in token_value()) {
        let splits = |value: &str| {
            value.bytes().any(|byte| matches!(byte, b'\0' | b'"') || model::isspace(byte))
        };
        for target in [format!("/share-0/{first}"), format!("/share-1/{second}")] {
            if splits(&target) {
                prop_assert!(validate_mount_target(&target).is_err(), "{:?}", target);
            }
        }
    }
}
