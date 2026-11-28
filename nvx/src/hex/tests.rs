use std::fmt;

use super::*;

#[test]
fn hexadecimal_text_is_written_in_lowercase() {
    struct Hex<'a>(&'a [u8]);

    impl fmt::Display for Hex<'_> {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write_hex(formatter, self.0)
        }
    }

    let bytes = [0x00, 0x0a, 0xb1, 0xff];
    assert_eq!(Hex(&bytes).to_string(), "000ab1ff");
    assert_eq!(Hex(&[]).to_string(), "");
    assert_eq!(hex(&bytes), "000ab1ff");
    assert_eq!(hex(&[]), "");
    let identity: [u8; 16] = std::array::from_fn(|index| u8::try_from(index * 17).unwrap());
    assert_eq!(hex(&identity), "00112233445566778899aabbccddeeff");
    assert_eq!(Hex(&identity).to_string(), hex(&identity));
}

#[test]
fn hexadecimal_text_is_read_in_either_case() {
    let bytes = [0x00, 0x0a, 0xb1, 0xff];
    assert_eq!(parse_hex::<4>("000ab1ff"), Some(bytes));
    assert_eq!(parse_hex::<4>("000AB1FF"), Some(bytes));
    assert_eq!(parse_hex::<2>(b"0aFf"), Some([0x0a, 0xff]));
    // An odd, short, or long length, a digit beyond `f`, a sign, and a non-ASCII byte.
    for text in [
        "000ab1f",
        "000ab1",
        "000ab1ff00",
        "000ab1fg",
        "+00ab1ff",
        "000ab1\u{e9}",
        "",
    ] {
        assert_eq!(parse_hex::<4>(text), None, "{text:?}");
        assert_eq!(parse_hex::<4>(text.as_bytes()), None, "{text:?}");
    }
    // The lengths of the host's identities: 32 digits for 16 bytes, 64 for 32.
    assert_eq!(parse_hex::<16>(&"Ab".repeat(16)), Some([0xab; 16]));
    assert_eq!(parse_hex::<16>(&"ab".repeat(32)), None);
    assert_eq!(parse_hex::<32>(&"cD".repeat(32)), Some([0xcd; 32]));
    assert_eq!(parse_hex::<32>(&"cd".repeat(16)), None);
    // The text that `hex` returns.
    let identity: [u8; 16] = std::array::from_fn(|index| u8::try_from(index * 17).unwrap());
    assert_eq!(parse_hex(hex(&identity)), Some(identity));
}
