use std::io;

use super::*;

#[test]
fn an_error_has_the_kind_that_it_was_created_with() {
    for kind in [
        SnapshotErrorKind::Io,
        SnapshotErrorKind::Malformed,
        SnapshotErrorKind::Incompatible,
        SnapshotErrorKind::CaptureFailed,
    ] {
        assert_eq!(SnapshotError::new(kind, "a refused snapshot").kind(), kind);
    }
}

#[test]
fn an_error_displays_its_message_and_exposes_its_source() {
    let message = "cannot read the manifest of the snapshot in /snapshots/a";
    let error = SnapshotError::new(SnapshotErrorKind::Io, message);
    assert_eq!(error.to_string(), message);
    assert!(std::error::Error::source(&error).is_none());

    let error = error.with_source(io::Error::new(io::ErrorKind::NotFound, "missing"));
    assert_eq!(error.kind(), SnapshotErrorKind::Io);
    assert_eq!(error.to_string(), message);
    let source = std::error::Error::source(&error).unwrap();
    let cause = source.downcast_ref::<io::Error>().unwrap();
    assert_eq!(cause.kind(), io::ErrorKind::NotFound);
    assert_eq!(cause.to_string(), "missing");
}
