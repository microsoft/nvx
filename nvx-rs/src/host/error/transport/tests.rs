use std::io;

use super::*;

#[test]
fn an_error_has_the_kind_that_it_was_created_with() {
    for kind in [
        TransportErrorKind::TimedOut,
        TransportErrorKind::Exited,
        TransportErrorKind::Refused,
        TransportErrorKind::Closed,
        TransportErrorKind::Reset,
        TransportErrorKind::Protocol,
        TransportErrorKind::Io,
    ] {
        assert_eq!(
            TransportError::new(kind, "a failed connection").kind(),
            kind
        );
    }
}

#[test]
fn an_error_displays_its_message_and_exposes_its_source() {
    let message = "the transport to the control endpoint failed";
    let error = TransportError::new(TransportErrorKind::Io, message);
    assert_eq!(error.to_string(), message);
    assert!(std::error::Error::source(&error).is_none());

    let error = error.with_source(io::Error::new(io::ErrorKind::ConnectionReset, "reset"));
    assert_eq!(error.kind(), TransportErrorKind::Io);
    assert_eq!(error.to_string(), message);
    let source = std::error::Error::source(&error).unwrap();
    let cause = source.downcast_ref::<io::Error>().unwrap();
    assert_eq!(cause.kind(), io::ErrorKind::ConnectionReset);
    assert_eq!(cause.to_string(), "reset");
}
