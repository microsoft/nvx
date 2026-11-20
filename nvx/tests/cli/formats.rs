//! The sole typed provision record and its start reader, and refusal of old records.

use serde_json::{Value, json};

use crate::harness::{LAYER, Root, SCRATCH, UUID, entries, json};
use crate::lifecycle::{PROVISIONED, provision, sandbox};
use crate::openvmm::{Options, assert_opened_distro, assert_opened_images, assert_stable_blocks};

#[test]
fn sandbox_default_and_explicit_x86_write_the_same_legacy_configuration() {
    let root = Root::new();
    root.nvx(&provision("{root/default}", &[]))
        .assert_silent_success();
    root.nvx(&provision("{root/explicit}", &["--architecture", "x86_64"]))
        .assert_silent_success();
    let default = json(&root.below("default/config.json"));
    let explicit = json(&root.below("explicit/config.json"));
    assert_eq!(default, explicit);
    assert_eq!(default["format"], 10);
    assert!(default.get("architecture").is_none());
}

#[cfg(all(windows, target_arch = "aarch64"))]
#[test]
fn explicit_arm_sandboxes_keep_their_isa_across_cli_processes() {
    use crate::harness::PROVENANCE;

    for scratch in [true, false] {
        let root = Root::new();
        let mut header = [0u8; 64];
        header[16..24].copy_from_slice(&64u64.to_le_bytes());
        header[24..32].copy_from_slice(&10u64.to_le_bytes());
        header[56..60].copy_from_slice(b"ARM\x64");
        root.write("build/aarch64/Image", header);
        root.write("build/aarch64/initramfs.cpio.gz", b"ARM initramfs\n");
        root.write(
            "build/aarch64/initramfs.cpio.gz.packages.json",
            r#"{"architecture":"aarch64"}"#,
        );
        for (name, member, digest) in PROVENANCE.into_iter().skip(1) {
            let name = name.replace("vmlinux", "Image");
            root.write(
                &format!("build/aarch64/{name}"),
                json!({"format":1, (member):digest}).to_string(),
            );
        }
        let mut options = vec!["--architecture", "aarch64", "--layer", LAYER];
        if scratch {
            options.extend(["--scratch", SCRATCH]);
        } else {
            options.extend(["--upper", "ram"]);
        }
        root.nvx(&sandbox("provision", "{root/arm}", &options))
            .assert_silent_success();
        let record = json(&root.below("arm/config.json"));
        assert_eq!(record["format"], 11);
        assert_eq!(record["architecture"], "aarch64");
        for value in [None, Some(Value::Null), Some(json!("unknown"))] {
            let mut malformed = record.clone();
            if let Some(value) = value {
                malformed["architecture"] = value;
            } else {
                malformed.as_object_mut().unwrap().remove("architecture");
            }
            root.write("arm/config.json", malformed.to_string());
            root.nvx(&sandbox::<&str>("start", "{root/arm}", &[]))
                .assert_error(
                    "format-unsupported",
                    &["requires an explicit supported architecture"],
                );
            assert!(root.openvmm_records().is_empty());
        }
        root.write("arm/config.json", record.to_string());
        root.nvx(&sandbox::<&str>("start", "{root/arm}", &[]))
            .assert_silent_success();
        let opened = Options::parse(&root.openvmm_arguments("build/aarch64/fake-openvmm-arm.json"));
        assert_eq!(
            opened.value("--kernel"),
            root.expand("{root/build/aarch64/Image}")
        );
        assert_eq!(opened.value("--cpu-profile"), "auto");
        assert_eq!(
            json(&root.below("arm/runtime.json"))["architecture"],
            "aarch64"
        );
        let mut contradictory = record.clone();
        contradictory["format"] = json!(10);
        contradictory
            .as_object_mut()
            .unwrap()
            .remove("architecture");
        root.write("arm/config.json", contradictory.to_string());
        root.nvx(&sandbox(
            "exec",
            "{root/arm}",
            &["--entrypoint", "/bin/true"],
        ))
        .assert_error("runtime-mismatch", &["different guest architectures"]);
        root.write("arm/config.json", record.to_string());
        let result = root.nvx(&sandbox(
            "exec",
            "{root/arm}",
            &["--entrypoint", "/bin/echo", "--arg", "arm-record"],
        ));
        result.assert_success();
        assert_eq!(result.stdout(), "arm-record\n");
        for operation in ["stop", "deprovision"] {
            root.nvx(&sandbox::<&str>(operation, "{root/arm}", &[]))
                .assert_silent_success();
        }
        assert!(!root.below("arm").exists());
    }
}

#[test]
fn configurations_of_every_format_start_and_exec() {
    for format in ["erofs", "gpt"] {
        for (upper, cap, expected_cap) in [
            ("scratch", None, None),
            ("ram", None, Some(50)),
            ("ram", Some("1"), Some(1)),
            ("ram", Some("90"), Some(90)),
        ] {
            let context = format!("{format}/{upper}/{cap:?}");
            let root = Root::new();
            let mut options = vec!["--layer", LAYER, "--layer-format", format, "--upper", upper];
            if upper == "scratch" {
                options.extend(["--scratch", SCRATCH]);
            }
            if let Some(cap) = cap {
                options.extend(["--ram-upper-percent", cap]);
            }
            root.nvx(&sandbox("provision", "{root/state}", &options))
                .assert_silent_success();
            let record = json(&root.below("state/config.json"));
            assert_eq!(record["format"], 10, "{context}");
            assert_eq!(
                record["image"],
                root.expand_json(&json!({
                    "format": format, "path": "{root/distro.erofs}", "identity": UUID,
                })),
                "{context}"
            );
            let expected_upper = match expected_cap {
                Some(cap) => json!({"kind": "ram", "max_memory_percent": cap}),
                None => {
                    root.expand_json(&json!({"kind": "scratch", "path": "{root/scratch.ext4}"}))
                }
            };
            assert_eq!(record["upper"], expected_upper, "{context}");
            assert_eq!(record["distro_binding"]["format"], format, "{context}");
            assert!(record.get("layers").is_none(), "{context}");
            assert!(record.get("scratch").is_none(), "{context}");
            assert_eq!(record["scratch_binding"].is_null(), upper == "ram");

            root.nvx(&sandbox::<&str>("start", "{root/state}", &[]))
                .assert_silent_success();
            let mut opened =
                Options::parse(&root.openvmm_arguments("build/fake-openvmm-state.json"));
            assert_stable_blocks(
                &root,
                &opened.take("--microvm-sandbox-block"),
                upper == "scratch",
            );
            if upper == "scratch" {
                assert_opened_images(&root, "state");
            } else {
                assert_opened_distro(&root, "state");
            }
            let line = opened.value("--cmdline");
            let tokens: Vec<_> = line.split_ascii_whitespace().collect();
            assert!(
                tokens.contains(&format!("nvx_layer=distro,0xd0003000,{format}:{UUID}").as_str()),
                "{context}: {line}"
            );
            if let Some(cap) = expected_cap {
                assert!(tokens.contains(&format!("nvx_ram_upper_percent={cap}").as_str()));
                assert!(tokens.contains(&"nvx_overlay_upper=ramfs"));
                assert!(!tokens.iter().any(|token| token.starts_with("nvx_scratch=")));
            } else {
                assert!(tokens.contains(&"nvx_scratch=0xd0006000,ext4"));
                assert!(
                    !tokens
                        .iter()
                        .any(|token| token.starts_with("nvx_ram_upper_percent="))
                );
            }
            assert!(opened.values("--microvm-lifecycle").is_empty());
            let outcome = root.nvx(&sandbox(
                "exec",
                "{root/state}",
                &["--entrypoint", "/bin/echo", "--arg", "hello"],
            ));
            outcome.assert_success();
            assert_eq!(outcome.stdout(), "hello\n", "{context}");
            for operation in ["stop", "deprovision"] {
                root.nvx(&sandbox::<&str>(operation, "{root/state}", &[]))
                    .assert_silent_success();
            }
            assert!(!root.below("state").exists(), "{context}");
        }
    }
}

#[test]
fn old_configuration_formats_are_refused_without_migration() {
    for text in [
        include_str!("../fixtures/state/format-1.json"),
        include_str!("../fixtures/state/format-1-network.json"),
        include_str!("../fixtures/state/format-1-nvx-py.json"),
        include_str!("../fixtures/state/format-2.json"),
        include_str!("../fixtures/state/format-3.json"),
        include_str!("../fixtures/state/format-4.json"),
        include_str!("../fixtures/state/format-5.json"),
        include_str!("../fixtures/state/format-6.json"),
        include_str!("../fixtures/state/format-7.json"),
        include_str!("../fixtures/state/format-7-without-shares.json"),
        r#"{"format":8}"#,
        // The format before the CLI recorded its own network options.
        r#"{"format":9}"#,
    ] {
        let root = Root::new();
        root.nvx(&provision("{root/state}", &[]))
            .assert_silent_success();
        root.write("state/config.json", text);
        let before = root.snapshot();
        root.nvx(&sandbox::<&str>("start", "{root/state}", &[]))
            .assert_error(
                "format-unsupported",
                &[
                    "unsupported format",
                    "deprovision the sandbox and provision it again",
                ],
            );
        assert_eq!(root.snapshot(), before);
        assert_eq!(entries(&root.below("state")), PROVISIONED);
        root.nvx(&sandbox::<&str>("deprovision", "{root/state}", &[]))
            .assert_silent_success();
    }
}

#[test]
fn invalid_recorded_roots_are_refused_before_spawn() {
    for (field, value) in [
        (
            "image",
            json!({"format": "ext4", "path": "image", "identity": UUID}),
        ),
        ("image", json!({"format": "gpt", "identity": UUID})),
        ("upper", json!({"kind": "ram", "max_memory_percent": 0})),
        ("upper", json!({"kind": "ram", "max_memory_percent": 91})),
        ("upper", json!({"kind": "ram", "max_memory_percent": "50"})),
        ("upper", json!({"kind": "ram", "max_memory_percent": 1.5})),
        ("upper", json!({"kind": "ram"})),
        (
            "upper",
            json!({"kind": "ram", "max_memory_percent": 50, "path": "scratch"}),
        ),
        (
            "upper",
            json!({"kind": "scratch", "path": "scratch", "max_memory_percent": 50}),
        ),
    ] {
        let root = Root::new();
        root.nvx(&provision("{root/state}", &[]))
            .assert_silent_success();
        let mut record = json(&root.below("state/config.json"));
        record[field] = value.clone();
        record["scratch_binding"] = Value::Null;
        root.write("state/config.json", record.to_string());
        let before = root.snapshot();
        root.nvx(&sandbox::<&str>("start", "{root/state}", &[]))
            .assert_error("io", &["sandbox configuration is malformed"]);
        assert_eq!(root.snapshot(), before, "{field}: {value}");
        assert!(!root.below("build/fake-openvmm-state.json").exists());
        root.nvx(&sandbox::<&str>("deprovision", "{root/state}", &[]))
            .assert_silent_success();
    }
}
