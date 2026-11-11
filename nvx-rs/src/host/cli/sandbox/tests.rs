//! Tests of the options of `nvx-rs sandbox`.

use proptest::prelude::*;

pub(in crate::host::cli) use super::options::{OPTIONS, SYNTAX};
use super::*;
use crate::host::sandbox::mount::PolicyKind;
use crate::host::sandbox::options::PolicyPathOption;

#[test]
fn exec_wait_defaults_to_sixty_seconds_and_uses_timeout_number_rules() {
    assert_eq!(parse(&["exec".to_owned()]).unwrap().0.wait, 60.0);
    let (options, unknown) =
        parse(&["exec", "--wait", "0.05", "--timeout", "2"].map(str::to_owned)).unwrap();
    assert!(unknown.is_empty());
    assert_eq!((options.wait, options.timeout), (0.05, 2.0));
    for value in ["0", "-1", "nan", "inf", "1e400", "invalid"] {
        let error = parse(&["exec".to_owned(), "--wait".to_owned(), value.to_owned()]).unwrap_err();
        assert_eq!(error.code(), Some(ErrorCode::ArgumentInvalid));
        assert!(error.message().unwrap().contains("--wait"));
    }
}

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
fn image_options_are_only_valid_for_run_and_provision() {
    for operation in ["start", "exec", "stop", "deprovision"] {
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
                    .contains("only valid for sandbox run or provision")
            );
        }
    }
    for operation in ["run", "provision"] {
        let (options, _) = parse(&[
            operation.to_owned(),
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
    }
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
