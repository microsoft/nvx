use super::*;

#[test]
fn the_crc_matches_the_ieee_check_values() {
    // The standard check value of CRC-32/ISO-HDLC, and two values that zlib computes.
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    assert_eq!(crc32(b""), 0);
    assert_eq!(crc32(&[0; 4]), 0x2144_DF1C);
    assert_eq!(
        crc32(b"The quick brown fox jumps over the lazy dog"),
        0x414F_A339
    );
}

#[test]
fn a_crc_over_parts_equals_one_over_the_whole() {
    let bytes: Vec<u8> = (0..=255).cycle().take(1000).collect();
    let parts = Crc32::new()
        .update(&bytes[..1])
        .update(&bytes[1..500])
        .update(&[])
        .update(&bytes[500..])
        .finish();
    assert_eq!(parts, crc32(&bytes));
}
