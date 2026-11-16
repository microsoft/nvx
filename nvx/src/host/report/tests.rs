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
        read(&path).unwrap().unwrap(),
        Report {
            category: Some("success".to_owned()),
            status_code: Some(0),
            teardown_complete: Some(true),
        }
    );
    fs::write(&path, r#"{"teardown":{"workers":true,"memory":false}}"#).unwrap();
    assert_eq!(
        read(&path).unwrap().unwrap(),
        Report {
            teardown_complete: Some(false),
            ..Report::default()
        }
    );
}

#[test]
fn missing_reports_are_distinct_from_malformed_or_unreadable_reports() {
    let directory = tempfile::tempdir().unwrap();
    assert!(
        read(&directory.path().join("missing.json"))
            .unwrap()
            .is_none()
    );
    let path = directory.path().join("outcome.json");
    fs::write(&path, b"{ truncated").unwrap();
    let malformed = read(&path).unwrap_err();
    assert_eq!(malformed.kind(), crate::ErrorKind::Io);
    assert!(
        std::error::Error::source(&malformed)
            .unwrap()
            .is::<serde_json::Error>()
    );
    let unreadable = read(directory.path()).unwrap_err();
    assert_eq!(unreadable.kind(), crate::ErrorKind::Io);
    assert!(
        std::error::Error::source(&unreadable)
            .unwrap()
            .is::<std::io::Error>()
    );
}

#[cfg(windows)]
#[test]
fn a_report_opened_without_read_sharing_returns_its_io_error() {
    use std::os::windows::fs::OpenOptionsExt;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("outcome.json");
    fs::write(&path, b"{}").unwrap();
    let held = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&path)
        .unwrap();
    let error = read(&path).unwrap_err();
    assert_eq!(error.kind(), crate::ErrorKind::Io);
    assert!(
        std::error::Error::source(&error)
            .unwrap()
            .is::<std::io::Error>()
    );
    drop(held);
    assert_eq!(read(&path).unwrap(), Some(Report::default()));
}
