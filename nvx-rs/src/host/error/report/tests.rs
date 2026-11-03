use super::*;

#[test]
fn an_error_has_the_kind_that_it_was_created_with() {
    for kind in [ReportErrorKind::Io, ReportErrorKind::Malformed] {
        assert_eq!(ReportError::new(kind, "an unreadable report").kind(), kind);
    }
}

#[test]
fn an_error_displays_its_message_and_exposes_its_source() {
    let message = "the outcome report /vms/a/outcome.json is not JSON";
    let error = ReportError::new(ReportErrorKind::Malformed, message);
    assert_eq!(error.to_string(), message);
    assert!(std::error::Error::source(&error).is_none());

    let cause = serde_json::from_str::<serde_json::Value>("{ truncated").unwrap_err();
    let cause_text = cause.to_string();
    let error = error.with_source(cause);
    assert_eq!(error.kind(), ReportErrorKind::Malformed);
    assert_eq!(error.to_string(), message);
    let source = std::error::Error::source(&error).unwrap();
    let cause = source.downcast_ref::<serde_json::Error>().unwrap();
    assert_eq!(cause.to_string(), cause_text);
}
