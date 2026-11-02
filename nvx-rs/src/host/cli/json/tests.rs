use serde_json::json;

use super::*;

#[test]
fn values_print_with_sorted_members_and_two_spaces() {
    let value = json!({
        "zeta": [1, null],
        "alpha": true,
        "empty": [],
        "nested": {"b": "x", "a": false},
        "none": {},
        "big": u64::MAX,
        "text": "a\"b\\c\n\u{7f}é\u{1f600}",
    });
    assert_eq!(
        pretty(&value),
        "{\n  \"alpha\": true,\n  \"big\": 18446744073709551615,\n  \"empty\": [],\n  \
             \"nested\": {\n    \"a\": false,\n    \"b\": \"x\"\n  },\n  \"none\": {},\n  \
             \"text\": \"a\\\"b\\\\c\\n\u{7f}é\u{1f600}\",\n  \"zeta\": [\n    1,\n    null\n  \
             ]\n}"
    );
    assert_eq!(pretty(&json!([])), "[]");
}

#[test]
fn objects_are_read_strictly_with_their_versions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("record.json");
    let read = |text: &[u8]| {
        std::fs::write(&path, text).unwrap();
        read_object(&path, "record", "format", &[1, 7])
            .map_err(|exit| (exit.code().unwrap(), exit.message().unwrap().to_owned()))
    };
    let shown = path.display();
    assert_eq!(
        read(br#"{"format": 7, "x": [1]}"#).unwrap()["x"],
        json!([1])
    );
    let unreadable = |detail: &str| {
        (
            ErrorCode::Io,
            format!("failed to read record: {shown}{detail}"),
        )
    };
    assert_eq!(read(b"{"), Err(unreadable("")));
    assert_eq!(read(b"\xff"), Err(unreadable("")));
    assert_eq!(read(b"{\"format\": NaN}"), Err(unreadable("")));
    assert_eq!(
        read(br#"{"format": 1, "format": 1}"#),
        Err(unreadable(": record has duplicate field \"format\""))
    );
    assert_eq!(
        read(br#"{"format": 1, "layers": [{"a": 1, "a": 2}]}"#),
        Err(unreadable(": layers[0] has duplicate field \"a\""))
    );
    let deep = format!(
        "{}{}",
        "[".repeat(MAX_JSON_DEPTH + 1),
        "]".repeat(MAX_JSON_DEPTH + 1)
    );
    let (code, message) = read(deep.as_bytes()).unwrap_err();
    assert_eq!(code, ErrorCode::Io);
    assert!(message.contains("nest more than 32 deep"), "{message}");
    let unsupported = Err((
        ErrorCode::FormatUnsupported,
        format!("record has an unsupported format: {shown}"),
    ));
    for text in [
        &br#"[]"#[..],
        br#"{}"#,
        br#"{"format": 2}"#,
        br#"{"format": 1.0}"#,
        br#"{"format": true}"#,
        br#"{"format": "1"}"#,
        br#"{"format": -1}"#,
    ] {
        assert_eq!(read(text), unsupported, "{}", String::from_utf8_lossy(text));
    }
    std::fs::remove_file(&path).unwrap();
    assert_eq!(
        read_object(&path, "record", "format", &[1])
            .unwrap_err()
            .code(),
        Some(ErrorCode::Io)
    );
}
