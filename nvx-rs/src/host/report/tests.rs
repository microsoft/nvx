use super::*;

#[test]
fn reports_are_summarized_without_free_form_fields() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("outcome.json");
    fs::write(
        &path,
        r#"{"schema_version":1,"outcome":{"operation":"managed","category":"success",
               "status_code":0},"teardown":{"workers":true,"memory":true}}"#,
    )
    .unwrap();
    assert_eq!(
        read(&path).unwrap(),
        Report {
            category: Some("success".to_owned()),
            status_code: Some(0),
            teardown_complete: Some(true),
        }
    );
    fs::write(&path, r#"{"teardown":{"workers":true,"memory":false}}"#).unwrap();
    assert_eq!(
        read(&path).unwrap(),
        Report {
            teardown_complete: Some(false),
            ..Report::default()
        }
    );
    assert!(read(&directory.path().join("missing.json")).is_none());
    fs::write(&path, b"{ truncated").unwrap();
    assert!(read(&path).is_none());
}
