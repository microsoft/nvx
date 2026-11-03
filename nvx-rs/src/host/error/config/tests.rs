use std::net::{AddrParseError, Ipv4Addr};

use super::*;

#[test]
fn an_error_has_the_kind_that_it_was_created_with() {
    for kind in [ConfigErrorKind::Invalid, ConfigErrorKind::Unsupported] {
        assert_eq!(ConfigError::new(kind, "a refused value").kind(), kind);
    }
}

#[test]
fn an_error_displays_its_message_and_exposes_its_source() {
    let message = "the guest network 10.0.0.256/24 is not an IPv4 address with a prefix length";
    let error = ConfigError::new(ConfigErrorKind::Invalid, message);
    assert_eq!(error.to_string(), message);
    assert!(std::error::Error::source(&error).is_none());

    let cause = "10.0.0.256".parse::<Ipv4Addr>().unwrap_err();
    let error = error.with_source(cause.clone());
    assert_eq!(error.kind(), ConfigErrorKind::Invalid);
    assert_eq!(error.to_string(), message);
    let source = std::error::Error::source(&error).unwrap();
    assert_eq!(source.downcast_ref::<AddrParseError>(), Some(&cause));
}
