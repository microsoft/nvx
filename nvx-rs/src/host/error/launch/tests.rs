use std::io;

use super::*;

#[test]
fn an_error_has_the_kind_that_it_was_created_with() {
    for kind in [
        LaunchErrorKind::Unavailable,
        LaunchErrorKind::Refused,
        LaunchErrorKind::Io,
        LaunchErrorKind::Exited,
        LaunchErrorKind::RuntimeMismatch,
        LaunchErrorKind::StillRunning,
    ] {
        assert_eq!(LaunchError::new(kind, "a failed launch").kind(), kind);
    }
}

#[test]
fn an_error_displays_its_message_and_exposes_its_source() {
    let message = "cannot create the log of the VM in /vms/a";
    let error = LaunchError::new(LaunchErrorKind::Io, message);
    assert_eq!(error.to_string(), message);
    assert!(std::error::Error::source(&error).is_none());

    let error = error.with_source(io::Error::new(io::ErrorKind::PermissionDenied, "denied"));
    assert_eq!(error.kind(), LaunchErrorKind::Io);
    assert_eq!(error.to_string(), message);
    let source = std::error::Error::source(&error).unwrap();
    let cause = source.downcast_ref::<io::Error>().unwrap();
    assert_eq!(cause.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(cause.to_string(), "denied");
}
