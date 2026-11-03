use std::io;

use super::*;

#[test]
fn an_error_has_the_kind_that_it_was_created_with() {
    for kind in [
        RestoreErrorKind::OperationFailed,
        RestoreErrorKind::Expired,
        RestoreErrorKind::Cancelled,
        RestoreErrorKind::Exited,
        RestoreErrorKind::Protocol,
    ] {
        assert_eq!(RestoreError::new(kind, "a failed restore").kind(), kind);
    }
}

#[test]
fn an_error_displays_its_message_and_exposes_its_source() {
    let message = "the connection to the VM's control endpoint failed during the restore";
    let error = RestoreError::new(RestoreErrorKind::OperationFailed, message);
    assert_eq!(error.to_string(), message);
    assert!(std::error::Error::source(&error).is_none());

    let error = error.with_source(io::Error::new(io::ErrorKind::ConnectionRefused, "refused"));
    assert_eq!(error.kind(), RestoreErrorKind::OperationFailed);
    assert_eq!(error.to_string(), message);
    let source = std::error::Error::source(&error).unwrap();
    let cause = source.downcast_ref::<io::Error>().unwrap();
    assert_eq!(cause.kind(), io::ErrorKind::ConnectionRefused);
    assert_eq!(cause.to_string(), "refused");
}
