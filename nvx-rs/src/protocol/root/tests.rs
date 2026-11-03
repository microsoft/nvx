//! Tests of the boot tokens of a layered root. `tests/fixtures/root-tokens.json` holds the command
//! lines that every reader of the tokens must agree on, which the tests of the shell init replay
//! too.

use serde_json::Value;

use super::*;
use crate::contract;

/// The command lines that every reader of the boot tokens agrees on.
const FIXTURE: &str = include_str!("../../../tests/fixtures/root-tokens.json");
/// The filesystem UUID of the tests' EROFS images.
const EROFS_UUID: &str = "0123abcd-4567-89ef-0123-456789abcdef";
/// The disk GUID of the tests' GPT images.
const GPT_GUID: &str = "fedcba98-7654-3210-fedc-ba9876543210";

fn uuid(text: &str) -> Uuid {
    Uuid::parse(text).unwrap()
}

fn layout(image: RootImage, upper: RootUpper) -> RootLayout {
    RootLayout { image, upper }
}

fn parse(command_line: &str) -> Result<Option<RootLayout>, String> {
    RootLayout::parse(command_line.as_bytes()).map_err(ProtocolError::into_message)
}

/// Returns the layout of a case of the fixture, or `None` for its `null`.
fn recorded_layout(value: &Value) -> Option<RootLayout> {
    if value.is_null() {
        return None;
    }
    let format = ImageFormat::from_name(value["format"].as_str().unwrap()).unwrap();
    let identity = value["identity"].as_str().map(uuid);
    let upper = match value["upper"].as_str().unwrap() {
        "scratch" => RootUpper::Scratch,
        "ram" => RootUpper::Ram,
        other => panic!("the fixture names the unknown upper {other}"),
    };
    Some(layout(RootImage { format, identity }, upper))
}

#[test]
fn wire_values_keep_their_literals() {
    assert_eq!(DISTRO_MMIO_BASE, 0xd000_3000);
    assert_eq!(SCRATCH_MMIO_BASE, 0xd000_6000);
    assert_eq!(LAYER_TOKEN, "nvx_layer=");
    assert_eq!(SCRATCH_TOKEN, "nvx_scratch=");
    assert_eq!(RAM_UPPER_TOKEN, "nvx_overlay_upper=ramfs");
    assert_eq!(format!("{LAYER}="), LAYER_TOKEN);
    assert_eq!(format!("{SCRATCH}="), SCRATCH_TOKEN);
    assert_eq!(format!("{OVERLAY_UPPER}={RAM_UPPER}"), RAM_UPPER_TOKEN);
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
    for (expected, layer, upper) in cases {
        assert_eq!(expected.tokens(), [layer.clone(), upper.to_owned()]);
        // The guest reads the tokens in either order, among others.
        for command_line in [
            format!("{layer} {upper}"),
            format!("{upper} {layer}"),
            format!("quiet nvx_sandbox=1 {layer}\t{upper}\nnvx_entrypoint=/bin/sh"),
        ] {
            assert_eq!(parse(&command_line), Ok(Some(expected)), "{command_line}");
        }
    }
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
    // The corpus holds every combination of format, identity, and upper.
    for format in ["erofs", "gpt"] {
        for identity in [true, false] {
            for upper in ["scratch", "ram"] {
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
        }
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

#[test]
fn the_boot_contract_version_names_these_tokens() {
    // Runtime identities record the boot contract version, and a build stops, rather than
    // drives, a VM that a build of another version started. A change to these tokens, a new image
    // format or upper, or another way for the guest to assemble the root from them changes the
    // boot contract, and so its version, before the pins here change with it; see `contract`.
    assert_eq!(
        (
            contract::BOOT_CONTRACT_VERSION,
            contract::MICROVM_ABI_VERSION
        ),
        (4, 3),
        "a change to the boot tokens of a layered root or to the microVM ABI changes the boot \
         contract: increment BOOT_CONTRACT_VERSION, and update this test with it"
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
}
