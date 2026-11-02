//! Checks of the shared file helpers.

use super::*;

/// Verifies that decimal files hold exactly one plain number, and their failure outcomes.
#[test]
fn decimal_files_hold_one_plain_number() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("value");
    for (contents, expected) in [
        (&b"0\n"[..], DecimalFile::Value(0)),
        (b"1000\n", DecimalFile::Value(1000)),
        (b" \t42 \n", DecimalFile::Value(42)),
        (b"18446744073709551615\n", DecimalFile::Value(u64::MAX)),
        (b"18446744073709551616\n", DecimalFile::Malformed),
        (b"+7\n", DecimalFile::Malformed),
        (b"-1\n", DecimalFile::Malformed),
        (b"0x10\n", DecimalFile::Malformed),
        (b"0trailing\n", DecimalFile::Malformed),
        (b"1\n2\n", DecimalFile::Malformed),
        (b"\x0b7\n", DecimalFile::Malformed),
        (b"\n", DecimalFile::Malformed),
        (b"", DecimalFile::Malformed),
    ] {
        std::fs::write(&path, contents).unwrap();
        assert_eq!(
            read_decimal_file(&path),
            expected,
            "{}",
            contents.escape_ascii()
        );
    }
    assert_eq!(
        read_decimal_file(&directory.path().join("missing")),
        DecimalFile::Missing
    );
    assert_eq!(read_decimal_file(directory.path()), DecimalFile::Malformed);
}
