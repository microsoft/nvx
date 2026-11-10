//! Tests of the checks of a one-shot run.

use std::fs;

use proptest::prelude::*;

pub(in crate::host::cli) use super::options::{OPTIONS, SYNTAX};
use super::*;
use crate::host::paths::resolve;
use crate::host::sandbox::layer::{LayerRole, SandboxLayer};
use crate::host::sandbox::mount::PolicyKind;
use crate::host::sandbox::options::PolicyPathOption;

#[test]
fn the_upper_is_scratch_or_ram() {
    let upper = |value: &str| {
        parse(&["--upper".to_owned(), value.to_owned()]).map(|(options, _)| options.upper)
    };
    assert_eq!(parse(&[]).unwrap().0.upper, None);
    assert_eq!(upper("scratch").unwrap(), Some(RootUpper::Scratch));
    assert_eq!(upper("ram").unwrap(), Some(RootUpper::Ram));
    // The option names the RAM upper `ram`, whatever its token or the guest's mount call it.
    for value in ["ramfs", "tmpfs", "RAM", "", "scratch,ram"] {
        assert!(upper(value).is_err(), "{value:?}");
    }
    for cap in [1, 50, 90] {
        let (options, _) = parse(&[
            "--upper".to_owned(),
            "ram".to_owned(),
            "--ram-upper-percent".to_owned(),
            cap.to_string(),
        ])
        .unwrap();
        assert_eq!(options.ram_upper_percent, Some(cap));
    }
    for value in ["0", "91", "-1", "+50", "1.5", "x", "\u{ff15}\u{ff10}", ""] {
        assert!(
            parse(&["--ram-upper-percent".to_owned(), value.to_owned()]).is_err(),
            "{value:?}"
        );
    }
}

#[test]
fn provision_options_do_not_change_other_commands() {
    for operation in ["run", "start", "exec", "stop", "deprovision"] {
        for flag in ["--layer-format", "--ram-upper-percent"] {
            let value = if flag == "--layer-format" {
                "erofs"
            } else {
                "50"
            };
            let (options, _) =
                parse(&[operation.to_owned(), flag.to_owned(), value.to_owned()]).unwrap();
            let error = run(None, options).unwrap_err();
            assert!(
                error
                    .message()
                    .unwrap()
                    .contains("only supported by provision")
            );
        }
    }
    let (options, _) = parse(&[
        "provision".to_owned(),
        "--ram-upper-percent".to_owned(),
        "50".to_owned(),
    ])
    .unwrap();
    assert!(
        run(None, options)
            .unwrap_err()
            .message()
            .unwrap()
            .contains("requires --upper ram")
    );
    for (name, format) in [("erofs", DistroFormat::Erofs), ("gpt", DistroFormat::Gpt)] {
        let (options, _) = parse(&["--layer-format".to_owned(), name.to_owned()]).unwrap();
        assert_eq!(options.layer_format, Some(format));
    }
    assert!(parse(&["--layer-format".to_owned(), "ext4".to_owned()]).is_err());
}

proptest! {
    #[test]
    fn policy_option_sequences_record_the_preceding_mount(
        events in prop::collection::vec((any::<bool>(), 0_u8..3), 0..60),
    ) {
        let mut arguments = Vec::new();
        let mut expected = Vec::new();
        let mut mounts = 0_usize;
        for (index, (new_mount, kind)) in events.into_iter().enumerate() {
            if new_mount {
                arguments.extend(["--mount".to_owned(), format!("/share-{mounts},host-{mounts},rw")]);
                mounts += 1;
            }
            let (kind, option) = match kind {
                0 => (PolicyKind::Denied, "--mount-deny"),
                1 => (PolicyKind::Allowed, "--mount-allow"),
                _ => (PolicyKind::Writable, "--mount-write"),
            };
            let path = format!("path-{index}");
            arguments.extend([option.to_owned(), path.clone()]);
            expected.push(PolicyPathOption {
                kind,
                preceding_mount: mounts.checked_sub(1),
                path,
            });
        }
        let (options, unrecognized) = parse(&arguments).unwrap();
        prop_assert!(unrecognized.is_empty());
        prop_assert_eq!(options.mounts.len(), mounts);
        prop_assert_eq!(options.policy_paths, expected);
    }
}

#[test]
fn run_checks_again_that_its_shares_do_not_overlap() {
    let directory = tempfile::tempdir().unwrap();
    let root = resolve(directory.path()).unwrap();
    let path = |name: &str| root.join(name).to_str().unwrap().to_owned();
    for (name, content) in [
        ("distro.erofs", "distro"),
        ("scratch.ext4", "scratch"),
        ("settings.json", "{}"),
        ("output.log", ""),
    ] {
        fs::write(path(name), content).unwrap();
    }
    let mounts = sandbox_mounts(
        &[
            format!("/etc/settings.json,{}", path("settings.json")),
            format!("/var/log/output.log,{},rw", path("output.log")),
        ],
        &[],
        None,
    )
    .unwrap();
    validate_mounts(&mounts).unwrap();
    let launch = SandboxLaunch::new(
        vec![SandboxLayer::new(
            LayerRole::Distro,
            &path("distro.erofs"),
            "11111111-1111-1111-1111-111111111111",
        )],
        SandboxUpper::scratch(&path("scratch.ext4")),
        SandboxSettings::default(),
    )
    .unwrap()
    .with_export(plan_mounts(&mounts).unwrap())
    .unwrap();
    let binding = validate_sandbox_systemd_policy(&launch).unwrap();
    run_plan(&launch, &binding).unwrap();

    // A hard link that replaced the read-only file with the writable one while the run verified
    // its distro layer would export one file under both access modes; the export plan's own
    // checks pass, since a file share's root is not pinned.
    fs::remove_file(path("settings.json")).unwrap();
    fs::hard_link(path("output.log"), path("settings.json")).unwrap();
    launch.export().unwrap().verify().unwrap();
    let error = run_plan(&launch, &binding).unwrap_err();
    assert_eq!(
        error.code(),
        Some(crate::ErrorCode::Unsupported),
        "{error:?}"
    );
    let message = error.message().unwrap_or_default();
    assert!(
        message.contains("name the same directory or file"),
        "{message}"
    );
}
