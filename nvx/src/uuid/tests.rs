use super::*;

/// The EFI system partition type GUID, `c12a7328-f81f-11d2-ba4b-00a0c93ec93b`, as GPT stores
/// it.
const ESP_LE: [u8; 16] = [
    0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B,
];

/// The same GUID's bytes, in the order of its text.
const ESP: [u8; 16] = [
    0xC1, 0x2A, 0x73, 0x28, 0xF8, 0x1F, 0x11, 0xD2, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B,
];

#[test]
fn uuids_display_and_parse_their_canonical_text() {
    let esp = Uuid::from_bytes(ESP);
    assert_eq!(esp.as_bytes(), &ESP);
    assert_eq!(esp.to_string(), "c12a7328-f81f-11d2-ba4b-00a0c93ec93b");
    assert_eq!(
        format!("{esp:?}"),
        "Uuid(c12a7328-f81f-11d2-ba4b-00a0c93ec93b)"
    );
    assert_eq!(
        Uuid::parse("c12a7328-f81f-11d2-ba4b-00a0c93ec93b"),
        Some(esp)
    );
    assert_eq!(
        Uuid::parse(b"c12a7328-f81f-11d2-ba4b-00a0c93ec93b"),
        Some(esp)
    );
    assert_eq!("c12a7328-f81f-11d2-ba4b-00a0c93ec93b".parse(), Ok(esp));
    let text = "0fc63daf-8483-4772-8e79-3d69d8477de4";
    assert_eq!(Uuid::parse(text).unwrap().to_string(), text);
}

#[test]
fn only_the_canonical_text_parses() {
    for text in [
        "",
        "C12A7328-F81F-11D2-BA4B-00A0C93EC93B",
        "c12a7328-f81f-11d2-ba4b-00a0c93ec93B",
        "c12a7328f81f11d2ba4b00a0c93ec93b",
        "{c12a7328-f81f-11d2-ba4b-00a0c93ec93b}",
        "urn:uuid:c12a7328-f81f-11d2-ba4b-00a0c93ec93b",
        "c12a7328-f81f-11d2-ba4b-00a0c93ec93",
        "c12a7328-f81f-11d2-ba4b-00a0c93ec93bb",
        "c12a7328-f81f-11d2-ba4b-00a0c93ec93g",
        "c12a7328_f81f-11d2-ba4b-00a0c93ec93b",
        "c12a7328-f81f-11d2-ba4b00-a0c93ec93b",
        "+12a7328-f81f-11d2-ba4b-00a0c93ec93b",
        " c12a7328-f81f-11d2-ba4b-00a0c93ec93",
        "c12a7328-f81f-11d2-ba4b-00a0c93ec9\u{e9}",
    ] {
        assert_eq!(Uuid::parse(text), None, "{text:?}");
        assert_eq!(text.parse::<Uuid>(), Err(ParseUuidError(())), "{text:?}");
    }
    assert_eq!(
        ParseUuidError(()).to_string(),
        "not a UUID in its canonical text: 32 lowercase hexadecimal digits in groups of 8, 4, \
             4, 4, and 12, joined by hyphens"
    );
}

#[test]
fn the_efi_layout_reverses_the_first_three_groups() {
    let esp = Uuid::from_bytes_le(ESP_LE);
    assert_eq!(esp, Uuid::from_bytes(ESP));
    assert_eq!(esp.to_bytes_le(), ESP_LE);
    assert_eq!(esp.to_string(), "c12a7328-f81f-11d2-ba4b-00a0c93ec93b");
    let bytes: [u8; 16] = std::array::from_fn(|index| index as u8);
    assert_eq!(
        Uuid::from_bytes_le(bytes).as_bytes(),
        &[3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15]
    );
    assert_eq!(Uuid::from_bytes_le(bytes).to_bytes_le(), bytes);
}

#[test]
fn the_nil_uuid_has_only_zero_bits() {
    assert_eq!(
        Uuid::NIL.to_string(),
        "00000000-0000-0000-0000-000000000000"
    );
    assert_eq!(
        Uuid::parse("00000000-0000-0000-0000-000000000000"),
        Some(Uuid::NIL)
    );
    assert!(Uuid::NIL.is_nil());
    assert!(Uuid::from_bytes_le([0; 16]).is_nil());
    assert!(!Uuid::from_bytes(ESP).is_nil());
}
