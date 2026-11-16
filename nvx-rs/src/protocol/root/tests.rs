//! Tests of the boot tokens of a layered root. `tests/fixtures/root-tokens.json` holds the command
//! lines that every reader of the tokens must agree on.

use serde_json::Value;

use super::*;

/// The command lines that every reader of the boot tokens agrees on.
const FIXTURE: &str = include_str!("../../../tests/fixtures/root-tokens.json");
/// The filesystem UUID of the tests' EROFS images.
const EROFS_UUID: &str = "0123abcd-4567-89ef-0123-456789abcdef";
/// The disk GUID of the tests' GPT images.
const GPT_GUID: &str = "fedcba98-7654-3210-fedc-ba9876543210";

/// The keys of a layout of the fixture.
const LAYOUT_KEYS: [&str; 7] = [
    "format",
    "identity",
    "upper",
    "overlay",
    "journal",
    "gpt_first_partition_bound",
    "max_memory_percent",
];

fn uuid(text: &str) -> Uuid {
    Uuid::parse(text).unwrap()
}

/// Returns the layout of `image` over `upper`, which names no setting.
fn layout(image: RootImage, upper: RootUpper) -> RootLayout {
    RootLayout {
        image,
        upper,
        overlay: None,
        journal: None,
        gpt_first_partition_bound: None,
        max_memory_percent: None,
    }
}

fn parse(command_line: &str) -> Result<Option<RootLayout>, String> {
    RootLayout::parse(command_line.as_bytes()).map_err(ProtocolError::into_message)
}

/// Returns the layout of a case of the fixture, or `None` for its `null`. A layout that leaves a
/// setting out names none.
fn recorded_layout(value: &Value) -> Option<RootLayout> {
    if value.is_null() {
        return None;
    }
    for key in value.as_object().unwrap().keys() {
        assert!(
            LAYOUT_KEYS.contains(&key.as_str()),
            "the fixture names the unknown {key}"
        );
    }
    let format = ImageFormat::from_name(value["format"].as_str().unwrap()).unwrap();
    let identity = value["identity"].as_str().map(uuid);
    let upper = match value["upper"].as_str().unwrap() {
        "scratch" => RootUpper::Scratch,
        "ram" => RootUpper::Ram,
        other => panic!("the fixture names the unknown upper {other}"),
    };
    let overlay = value["overlay"].as_str().map(|mode| match mode {
        "trusted-lowers" => OverlayMode::TrustedLowers,
        "untrusted-lowers" => OverlayMode::UntrustedLowers,
        other => panic!("the fixture names the unknown overlay mode {other}"),
    });
    let journal = value["journal"].as_str().map(|journal| match journal {
        "allowed" => Journal::Allowed,
        "forbidden" => Journal::Forbidden,
        other => panic!("the fixture names the unknown journal policy {other}"),
    });
    Some(RootLayout {
        overlay,
        journal,
        gpt_first_partition_bound: value["gpt_first_partition_bound"].as_u64(),
        max_memory_percent: value["max_memory_percent"]
            .as_u64()
            .map(|percent| u8::try_from(percent).unwrap()),
        ..layout(RootImage { format, identity }, upper)
    })
}

#[test]
fn wire_values_keep_their_literals() {
    assert_eq!(DISTRO_MMIO_BASE, 0xd000_3000);
    assert_eq!(SCRATCH_MMIO_BASE, 0xd000_6000);
    assert_eq!(LAYER_TOKEN, "nvx_layer=");
    assert_eq!(SCRATCH_TOKEN, "nvx_scratch=");
    assert_eq!(RAM_UPPER_TOKEN, "nvx_overlay_upper=ramfs");
    assert_eq!(OVERLAY_MODE_TOKEN, "nvx_overlay_mode=");
    assert_eq!(SCRATCH_JOURNAL_TOKEN, "nvx_scratch_journal=");
    assert_eq!(FIRST_PARTITION_BOUND_TOKEN, "nvx_first_partition_bound=");
    assert_eq!(RAM_UPPER_PERCENT_TOKEN, "nvx_ram_upper_percent=");
    assert_eq!(format!("{LAYER}="), LAYER_TOKEN);
    assert_eq!(format!("{SCRATCH}="), SCRATCH_TOKEN);
    assert_eq!(format!("{OVERLAY_UPPER}={RAM_UPPER}"), RAM_UPPER_TOKEN);
    assert_eq!(format!("{OVERLAY_MODE}="), OVERLAY_MODE_TOKEN);
    assert_eq!(format!("{SCRATCH_JOURNAL}="), SCRATCH_JOURNAL_TOKEN);
    assert_eq!(
        format!("{FIRST_PARTITION_BOUND}="),
        FIRST_PARTITION_BOUND_TOKEN
    );
    assert_eq!(format!("{RAM_UPPER_PERCENT}="), RAM_UPPER_PERCENT_TOKEN);
    assert_eq!(
        OVERLAY_MODES.map(overlay_name),
        ["trusted-lowers", "untrusted-lowers"]
    );
    assert_eq!(JOURNALS.map(journal_name), ["allowed", "forbidden"]);
    assert_eq!(RAM_UPPER_CAPS, 1..=90);
    assert_eq!(ImageFormat::Erofs.as_str(), "erofs");
    assert_eq!(ImageFormat::Gpt.to_string(), "gpt");
    assert_eq!(RootUpper::Scratch.as_str(), "scratch");
    assert_eq!(RootUpper::Ram.to_string(), "ram");
    for format in ImageFormat::ALL {
        assert_eq!(ImageFormat::from_name(format.as_str()), Some(format));
    }
    for name in ["", "EROFS", "Gpt", "erofs ", "ext4", "squashfs"] {
        assert_eq!(ImageFormat::from_name(name), None, "{name}");
    }
}

#[test]
fn every_layout_renders_its_tokens_and_reads_them_back() {
    let (erofs, gpt) = (Some(uuid(EROFS_UUID)), Some(uuid(GPT_GUID)));
    let scratch = "nvx_scratch=0xd0006000,ext4";
    let ram = "nvx_overlay_upper=ramfs";
    let cases = [
        (
            layout(RootImage::erofs(erofs), RootUpper::Scratch),
            format!("nvx_layer=distro,0xd0003000,erofs:{EROFS_UUID}"),
            scratch,
        ),
        (
            layout(RootImage::erofs(None), RootUpper::Scratch),
            "nvx_layer=distro,0xd0003000,erofs".to_owned(),
            scratch,
        ),
        (
            layout(RootImage::gpt(gpt), RootUpper::Scratch),
            format!("nvx_layer=distro,0xd0003000,gpt:{GPT_GUID}"),
            scratch,
        ),
        (
            layout(RootImage::gpt(None), RootUpper::Scratch),
            "nvx_layer=distro,0xd0003000,gpt".to_owned(),
            scratch,
        ),
        (
            layout(RootImage::erofs(erofs), RootUpper::Ram),
            format!("nvx_layer=distro,0xd0003000,erofs:{EROFS_UUID}"),
            ram,
        ),
        (
            layout(RootImage::erofs(None), RootUpper::Ram),
            "nvx_layer=distro,0xd0003000,erofs".to_owned(),
            ram,
        ),
        (
            layout(RootImage::gpt(gpt), RootUpper::Ram),
            format!("nvx_layer=distro,0xd0003000,gpt:{GPT_GUID}"),
            ram,
        ),
        (
            layout(RootImage::gpt(None), RootUpper::Ram),
            "nvx_layer=distro,0xd0003000,gpt".to_owned(),
            ram,
        ),
    ];
    // Each setting that a layout may name, or none, with its token: the overlay's mode, a
    // scratch's journal policy, a RAM upper's cap, and a GPT disk's bound.
    let overlays = [
        (None, None),
        (
            Some(OverlayMode::TrustedLowers),
            Some("nvx_overlay_mode=trusted-lowers"),
        ),
        (
            Some(OverlayMode::UntrustedLowers),
            Some("nvx_overlay_mode=untrusted-lowers"),
        ),
    ];
    let journals = [
        (None, None),
        (Some(Journal::Allowed), Some("nvx_scratch_journal=allowed")),
        (
            Some(Journal::Forbidden),
            Some("nvx_scratch_journal=forbidden"),
        ),
    ];
    let caps = [
        (None, None),
        (Some(1), Some("nvx_ram_upper_percent=1")),
        (Some(90), Some("nvx_ram_upper_percent=90")),
    ];
    let bounds = [
        (None, None),
        (Some(1), Some("nvx_first_partition_bound=1")),
        (
            Some(u64::MAX),
            Some("nvx_first_partition_bound=18446744073709551615"),
        ),
    ];
    let mut layouts = 0;
    for (bare, layer, upper) in cases {
        let (journals, caps) = match bare.upper {
            RootUpper::Scratch => (&journals[..], &[(None, None)][..]),
            RootUpper::Ram => (&[(None, None)][..], &caps[..]),
        };
        let bounds = match bare.image.format {
            ImageFormat::Erofs => &[(None, None)][..],
            ImageFormat::Gpt => &bounds[..],
        };
        for &(overlay, overlay_token) in &overlays {
            for &(journal, journal_token) in journals {
                for &(max_memory_percent, cap_token) in caps {
                    for &(gpt_first_partition_bound, bound_token) in bounds {
                        let expected = RootLayout {
                            overlay,
                            journal,
                            gpt_first_partition_bound,
                            max_memory_percent,
                            ..bare
                        };
                        let mut tokens = vec![layer.as_str(), upper];
                        tokens.extend(
                            [overlay_token, journal_token, bound_token, cap_token]
                                .into_iter()
                                .flatten(),
                        );
                        assert_eq!(expected.tokens(), tokens);
                        // The guest reads the tokens in any order, among others.
                        let reversed: Vec<&str> = tokens.iter().rev().copied().collect();
                        for command_line in [
                            tokens.join(" "),
                            reversed.join(" "),
                            format!(
                                "quiet nvx_sandbox=1 {}\nnvx_entrypoint=/bin/sh",
                                tokens.join("\t")
                            ),
                        ] {
                            assert_eq!(parse(&command_line), Ok(Some(expected)), "{command_line}");
                        }
                        layouts += 1;
                    }
                }
            }
        }
    }
    // Each of the 4 EROFS cases takes 3 overlay modes by 3 settings of its upper, and each of the
    // 4 GPT cases 3 bounds more.
    assert_eq!(layouts, 4 * 3 * 3 + 4 * 3 * 3 * 3);
}

#[test]
fn the_longest_tokens_are_those_of_an_identified_image_over_scratch() {
    // The command-line budgets of launches count these bytes: the image's token takes at most 70
    // bytes, and the upper's 27.
    let longest = layout(RootImage::erofs(Some(uuid(EROFS_UUID))), RootUpper::Scratch).tokens();
    assert_eq!([longest[0].len(), longest[1].len()], [70, 27]);
    for format in ImageFormat::ALL {
        for identity in [None, Some(uuid(GPT_GUID))] {
            for upper in [RootUpper::Scratch, RootUpper::Ram] {
                let tokens = layout(RootImage { format, identity }, upper).tokens();
                assert!(tokens[0].len() <= longest[0].len(), "{tokens:?}");
                assert!(tokens[1].len() <= longest[1].len(), "{tokens:?}");
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
                let expected = recorded_layout(&case["layout"]);
                assert_eq!(actual, Ok(expected), "{name}");
                // What the guest reads, the host renders.
                if let Some(expected) = expected {
                    assert_eq!(
                        parse(&expected.tokens().join(" ")),
                        Ok(Some(expected)),
                        "{name}"
                    );
                }
            }
        }
    }
    // The corpus holds every combination of format, identity, and upper, and every combination of
    // format, upper, and overlay mode with the settings that the format and the upper take.
    for format in ["erofs", "gpt"] {
        for upper in ["scratch", "ram"] {
            for identity in [true, false] {
                assert!(
                    cases.iter().any(|case| {
                        let layout = &case["layout"];
                        layout["format"] == format
                            && layout["identity"].is_string() == identity
                            && layout["upper"] == upper
                    }),
                    "the fixture lacks {format} with identity {identity} over {upper}"
                );
            }
            let setting = match upper {
                "scratch" => "journal",
                _ => "max_memory_percent",
            };
            for overlay in ["trusted-lowers", "untrusted-lowers"] {
                assert!(
                    cases.iter().any(|case| {
                        let layout = &case["layout"];
                        layout["format"] == format
                            && layout["upper"] == upper
                            && layout["overlay"] == overlay
                            && !layout[setting].is_null()
                            && (format == "erofs" || !layout["gpt_first_partition_bound"].is_null())
                    }),
                    "the fixture lacks {format} over {upper} for {overlay} with its settings"
                );
            }
        }
    }
    // And each journal policy.
    for journal in ["allowed", "forbidden"] {
        assert!(
            cases
                .iter()
                .any(|case| case["layout"]["journal"] == journal),
            "the fixture lacks the journal policy {journal}"
        );
    }
}

#[test]
fn parsing_reports_bytes_that_are_not_text() {
    assert_eq!(
        parse("nvx_layer=distro,0xd0003000,erofs:\u{e9} nvx_scratch=0xd0006000,ext4"),
        Err(
            "the nvx_layer kernel parameter names an invalid EROFS filesystem UUID: '\\xc3\\xa9'"
                .to_owned()
        )
    );
    assert_eq!(
        RootLayout::parse(b"nvx_layer=distro,0xd0003000,\xff nvx_scratch=0xd0006000,ext4")
            .unwrap_err()
            .message(),
        "the nvx_layer kernel parameter names the unsupported image format '\\xff'"
    );
    // So do the values of settings.
    for (command_line, message) in [
        (
            b"nvx_overlay_mode=\xff".as_slice(),
            "the nvx_overlay_mode kernel parameter must be trusted-lowers or untrusted-lowers, not \
             '\\xff'",
        ),
        (
            b"nvx_ram_upper_percent=9\xff".as_slice(),
            "the nvx_ram_upper_percent kernel parameter must be a decimal number from 1 to 90, \
             without a sign or leading zeros, not '9\\xff'",
        ),
    ] {
        assert_eq!(
            RootLayout::parse(command_line).unwrap_err().message(),
            message
        );
    }
    // Other tokens may hold any bytes.
    assert_eq!(
        RootLayout::parse(
            b"\xff\xfe=\x00 nvx_layer=distro,0xd0003000,erofs nvx_scratch=0xd0006000,ext4"
        )
        .unwrap(),
        Some(layout(RootImage::erofs(None), RootUpper::Scratch))
    );
}

/// Returns the name of `format` in the image's token, as the boot contract pins it. A new format
/// stops this match from compiling until the boot contract names it.
fn contracted_format(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Erofs => "erofs",
        ImageFormat::Gpt => "gpt",
    }
}

/// Returns the token of `upper`, as the boot contract pins it. A new upper stops this match from
/// compiling until the boot contract names it.
fn contracted_upper(upper: RootUpper) -> &'static str {
    match upper {
        RootUpper::Scratch => "nvx_scratch=0xd0006000,ext4",
        RootUpper::Ram => "nvx_overlay_upper=ramfs",
    }
}

/// Returns the token of `mode`, as the boot contract pins it. A new mode stops this match from
/// compiling until the boot contract names it.
fn contracted_overlay(mode: OverlayMode) -> &'static str {
    match mode {
        OverlayMode::TrustedLowers => "nvx_overlay_mode=trusted-lowers",
        OverlayMode::UntrustedLowers => "nvx_overlay_mode=untrusted-lowers",
    }
}

/// Returns the token of `journal`, as the boot contract pins it. A new policy stops this match
/// from compiling until the boot contract names it.
fn contracted_journal(journal: Journal) -> &'static str {
    match journal {
        Journal::Allowed => "nvx_scratch_journal=allowed",
        Journal::Forbidden => "nvx_scratch_journal=forbidden",
    }
}

#[test]
fn the_boot_contract_version_names_these_tokens() {
    // The CLI's runtime identities record its boot contract version, and the CLI stops, rather
    // than drives, a VM that a build of another version started. A change to these tokens, a new
    // image format or upper, or another way for the guest to assemble the root from them changes
    // the boot contract, and so its version, before the pins here change with it; see
    // `host::artifacts::compatibility`. The tokens of the root's settings joined version 5 while
    // nothing deployed rendered them.
    #[cfg(feature = "cli")]
    assert_eq!(
        crate::host::artifacts::compatibility::BOOT_CONTRACT_VERSION,
        5,
        "a change to the boot tokens of a layered root changes the boot contract: increment \
         BOOT_CONTRACT_VERSION, and update this test with it"
    );
    for format in ImageFormat::ALL {
        let image = format!("nvx_layer=distro,0xd0003000,{}", contracted_format(format));
        for identity in [None, Some(uuid(GPT_GUID))] {
            let image = match identity {
                Some(identity) => format!("{image}:{identity}"),
                None => image.clone(),
            };
            for upper in [RootUpper::Scratch, RootUpper::Ram] {
                assert_eq!(
                    layout(RootImage { format, identity }, upper).tokens(),
                    [image.clone(), contracted_upper(upper).to_owned()]
                );
            }
        }
    }
    // Each setting that a layout names adds its token after the upper's, in this order.
    for overlay in OVERLAY_MODES {
        for journal in JOURNALS {
            let scratch = RootLayout {
                overlay: Some(overlay),
                journal: Some(journal),
                gpt_first_partition_bound: Some(4096),
                ..layout(RootImage::gpt(None), RootUpper::Scratch)
            };
            assert_eq!(
                scratch.tokens()[2..],
                [
                    contracted_overlay(overlay),
                    contracted_journal(journal),
                    "nvx_first_partition_bound=4096",
                ]
            );
        }
        let ram = RootLayout {
            overlay: Some(overlay),
            max_memory_percent: Some(90),
            ..layout(RootImage::erofs(None), RootUpper::Ram)
        };
        assert_eq!(
            ram.tokens()[2..],
            [contracted_overlay(overlay), "nvx_ram_upper_percent=90"]
        );
    }
}
