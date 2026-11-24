use super::*;

#[test]
fn path_segments_are_encoded_as_python_quotes_them() {
    assert_eq!(encode("v0.1.0-dev.abc_~"), "v0.1.0-dev.abc_~");
    assert_eq!(encode("a b/c?d#e%"), "a%20b%2Fc%3Fd%23e%25");
    assert_eq!(encode("ü"), "%C3%BC");
}
