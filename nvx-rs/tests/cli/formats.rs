//! The sole typed provision record and its start reader, and refusal of old records.

use serde_json::{Value, json};

use crate::harness::{LAYER, Root, SCRATCH, UUID, entries, json};
use crate::lifecycle::{PROVISIONED, provision, sandbox};
use crate::openvmm::{Options, assert_opened_distro, assert_opened_images, assert_stable_blocks};

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
            root.nvx_rs(&sandbox("provision", "{root/state}", &options))
                .assert_silent_success();
            let record = json(&root.below("state/config.json"));
            assert_eq!(record["format"], 9, "{context}");
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

            root.nvx_rs(&sandbox::<&str>("start", "{root/state}", &[]))
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
            let outcome = root.nvx_rs(&sandbox(
                "exec",
                "{root/state}",
                &["--entrypoint", "/bin/echo", "--arg", "hello"],
            ));
            outcome.assert_success();
            assert_eq!(outcome.stdout(), "hello\n", "{context}");
            for operation in ["stop", "deprovision"] {
                root.nvx_rs(&sandbox::<&str>(operation, "{root/state}", &[]))
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
    ] {
        let root = Root::new();
        root.nvx_rs(&provision("{root/state}", &[]))
            .assert_silent_success();
        root.write("state/config.json", text);
        let before = root.snapshot();
        root.nvx_rs(&sandbox::<&str>("start", "{root/state}", &[]))
            .assert_error("format-unsupported", &["unsupported format"]);
        assert_eq!(root.snapshot(), before);
        assert_eq!(entries(&root.below("state")), PROVISIONED);
        root.nvx_rs(&sandbox::<&str>("deprovision", "{root/state}", &[]))
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
        root.nvx_rs(&provision("{root/state}", &[]))
            .assert_silent_success();
        let mut record = json(&root.below("state/config.json"));
        record[field] = value.clone();
        record["scratch_binding"] = Value::Null;
        root.write("state/config.json", record.to_string());
        let before = root.snapshot();
        root.nvx_rs(&sandbox::<&str>("start", "{root/state}", &[]))
            .assert_error("io", &["sandbox configuration is malformed"]);
        assert_eq!(root.snapshot(), before, "{field}: {value}");
        assert!(!root.below("build/fake-openvmm-state.json").exists());
        root.nvx_rs(&sandbox::<&str>("deprovision", "{root/state}", &[]))
            .assert_silent_success();
    }
}
