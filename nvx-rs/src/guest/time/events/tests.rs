use super::*;

#[test]
fn escaping_is_byte_oriented_and_never_splits_an_escape() {
    assert_eq!(escape_text(b"a\"\\\x01\xff\0", 32), r#"a\"\\\x01\xff\x00"#);
    assert_eq!(escape_text(b"\x01\x02", 7), r"\x01");
    assert_eq!(escape_text(b"ab\"", 3), "ab");
}

#[test]
fn event_has_the_abi_limit() {
    let line = format_event(
        false,
        ViolationCode::HungTask,
        EventSource::Watcher,
        Phase::Runtime,
        0,
        1,
        &[2; 400],
    );
    assert!(line.len() <= EVENT_MAX);
    assert!(line.ends_with("\\x02\"\n"));
}

#[test]
fn codes_and_sources_have_the_abi_names() {
    for (index, check) in [
        CheckId::C1,
        CheckId::C2,
        CheckId::C3,
        CheckId::C4,
        CheckId::C5,
        CheckId::C6,
        CheckId::C7,
        CheckId::C8,
        CheckId::C9,
        CheckId::C10,
        CheckId::C11,
        CheckId::C12,
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            ViolationCode::Conformance(check).to_string(),
            format!("G_CONFORMANCE_C{}", index + 1)
        );
    }
    for (code, name) in [
        (ViolationCode::RepairClock, "G_REPAIR_CLOCK"),
        (ViolationCode::RepairGeneration, "G_REPAIR_GENERATION"),
        (ViolationCode::RepairPacket, "G_REPAIR_PACKET"),
        (ViolationCode::RepairSample, "G_REPAIR_SAMPLE"),
        (ViolationCode::RepairSuppression, "G_REPAIR_SUPPRESSION"),
    ] {
        assert_eq!(code.to_string(), name);
    }
    assert_eq!(ViolationCode::KmsgOverrun.to_string(), "G_KMSG_OVERRUN");
    assert_eq!(EventSource::Repair.to_string(), "repair");
}
