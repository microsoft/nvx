//! Tests of the boot tokens of the host export. `tests/fixtures/export-tokens.json` holds the
//! command lines that every reader of the tokens must agree on.

use serde_json::Value;

use super::*;

/// The command lines that every reader of the export's tokens agrees on.
const FIXTURE: &str = include_str!("../../../tests/fixtures/export-tokens.json");

fn parse(command_line: &str) -> Result<Option<HostExport>, String> {
    HostExport::parse(command_line.as_bytes()).map_err(ProtocolError::into_message)
}

fn export(tag: &str, mode: ExportMode) -> HostExport {
    HostExport {
        tag: tag.to_owned(),
        mode,
    }
}

/// Returns the export of a case of the fixture, or `None` for its `null`.
fn recorded_export(value: &Value) -> Option<HostExport> {
    if value.is_null() {
        return None;
    }
    let mode = match value["mode"].as_str().unwrap() {
        "ro" => ExportMode::ReadOnly,
        "rw" => ExportMode::ReadWrite,
        other => panic!("the fixture names the unknown mode {other}"),
    };
    Some(export(value["tag"].as_str().unwrap(), mode))
}

/// Returns the tokens that OpenVMM appends for an aggregate export with `tag` and `mode`.
fn tokens(tag: &str, mode: ExportMode) -> String {
    format!("virtfs_dir={EXPORT_DIRECTORY} virtfs_tag={tag} virtfs_mode={mode} virtfs_aggregate=1")
}

#[test]
fn wire_values_keep_their_literals() {
    assert_eq!(EXPORT_DIRECTORY, "/run/nvx/hostfs/root");
    assert_eq!(
        PARAMETERS,
        [
            "virtfs_dir",
            "virtfs_tag",
            "virtfs_mode",
            "virtfs_aggregate"
        ]
    );
    assert_eq!(AGGREGATE_VALUE, "1");
    assert_eq!(SHARE, "nvx_share");
    assert_eq!(ExportMode::ReadOnly.as_str(), "ro");
    assert_eq!(ExportMode::ReadWrite.to_string(), "rw");
    for mode in ExportMode::ALL {
        assert_eq!(ExportMode::from_value(mode.as_str().as_bytes()), Some(mode));
    }
    for value in ["", "RO", "Rw", "ro ", "r", "rwx", "ro,rw"] {
        assert_eq!(ExportMode::from_value(value.as_bytes()), None, "{value}");
    }
}

#[test]
fn every_export_reads_back_from_its_tokens_among_others() {
    for mode in ExportMode::ALL {
        for tag in ["microvm", "fs0", "A-b_9"] {
            let tokens = tokens(tag, mode);
            for command_line in [
                tokens.clone(),
                format!("quiet nvx_sandbox=1 {tokens}\tnvx_maps=3\nnvx_entrypoint=/bin/sh"),
                tokens.split(' ').rev().collect::<Vec<_>>().join("\n"),
            ] {
                assert_eq!(
                    parse(&command_line),
                    Ok(Some(export(tag, mode))),
                    "{command_line}"
                );
            }
        }
    }
}

#[test]
fn every_reader_parses_the_recorded_command_lines_alike() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    let mut names = std::collections::HashSet::new();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        assert!(names.insert(name), "the fixture repeats the case {name}");
        let command_line = case["command_line"].as_str().unwrap();
        let actual = parse(command_line);
        match case.get("error") {
            Some(error) => assert_eq!(actual, Err(error.as_str().unwrap().to_owned()), "{name}"),
            None => {
                let expected = recorded_export(&case["export"]);
                assert_eq!(actual, Ok(expected.clone()), "{name}");
                // What OpenVMM appends for the export reads back as the same export.
                if let Some(expected) = expected {
                    assert_eq!(
                        parse(&tokens(&expected.tag, expected.mode)),
                        Ok(Some(expected)),
                        "{name}"
                    );
                }
            }
        }
    }
    // The corpus holds an export of each mode, no export, and a refusal of each token's form, of
    // a repeated token, of an incomplete export, and of a live share.
    for mode in ["ro", "rw"] {
        assert!(
            cases.iter().any(|case| case["export"]["mode"] == mode),
            "the fixture lacks an export of mode {mode}"
        );
    }
    assert!(
        cases
            .iter()
            .any(|case| case.get("error").is_none() && case["export"].is_null())
    );
    let errors: Vec<&str> = cases
        .iter()
        .filter_map(|case| case["error"].as_str())
        .collect();
    for parameter in PARAMETERS {
        for refusal in ["has no value", "is repeated", "must be"] {
            let expected = format!("the {parameter} kernel parameter {refusal}");
            assert!(
                errors.iter().any(|error| error.starts_with(&expected)),
                "the fixture lacks the refusal {expected}"
            );
        }
        assert!(
            errors.iter().any(|error| {
                error.starts_with("the host export is incomplete") && error.contains(parameter)
            }),
            "the fixture lacks an export without {parameter}"
        );
    }
    assert!(
        errors
            .iter()
            .any(|error| error.starts_with("the nvx_share kernel parameter"))
    );
}

#[test]
fn parsing_reports_bytes_that_are_not_text() {
    assert_eq!(
        parse(&tokens("micro\u{e9}vm", ExportMode::ReadWrite)),
        Err(
            "the virtfs_tag kernel parameter must be a tag of letters, digits, _, and -, not \
             'micro\\xc3\\xa9vm'"
                .to_owned()
        )
    );
    assert_eq!(
        HostExport::parse(b"virtfs_dir=/run/nvx/hostfs/\xff")
            .unwrap_err()
            .message(),
        "the virtfs_dir kernel parameter must be /run/nvx/hostfs/root, not '/run/nvx/hostfs/\\xff'"
    );
    assert_eq!(
        HostExport::parse(b"virtfs_mode=r\x00")
            .unwrap_err()
            .message(),
        "the virtfs_mode kernel parameter must be ro or rw, not 'r\\x00'"
    );
    // Other tokens may hold any bytes.
    let mut command_line = b"\xff\xfe=\x00 nvx_share\xff=1 ".to_vec();
    command_line.extend_from_slice(tokens("microvm", ExportMode::ReadOnly).as_bytes());
    assert_eq!(
        HostExport::parse(&command_line).unwrap(),
        Some(export("microvm", ExportMode::ReadOnly))
    );
}

#[test]
fn an_incomplete_export_names_what_it_lacks_in_order() {
    assert_eq!(alternatives(&["a"]), "a");
    assert_eq!(alternatives(&["a", "b"]), "a or b");
    assert_eq!(alternatives(&["a", "b", "c"]), "a, b, or c");
    // Each nonempty proper subset of the tokens is incomplete, and names the others in the order
    // in which OpenVMM appends them.
    let all = tokens("microvm", ExportMode::ReadWrite);
    let all: Vec<&str> = all.split(' ').collect();
    for subset in 1..(1 << all.len()) - 1 {
        let present: Vec<&str> = (0..all.len())
            .filter(|index| subset & (1 << index) != 0)
            .map(|index| all[index])
            .collect();
        let missing: Vec<&str> = (0..all.len())
            .filter(|index| subset & (1 << index) == 0)
            .map(|index| PARAMETERS[index])
            .collect();
        assert_eq!(
            parse(&present.join(" ")),
            Err(format!(
                "the host export is incomplete: the command line has no {} kernel parameter",
                alternatives(&missing)
            )),
            "{present:?}"
        );
    }
}
