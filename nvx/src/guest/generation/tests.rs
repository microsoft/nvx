use super::*;

#[test]
fn an_id_prints_as_lowercase_hexadecimal() {
    let bytes: [u8; GenerationId::LENGTH] = std::array::from_fn(|index| index as u8 * 17);
    let id = GenerationId::from_bytes(bytes);
    assert_eq!(id.to_bytes(), bytes);
    assert_eq!(id.to_string(), "00112233445566778899aabbccddeeff");
    assert_eq!(
        format!("{id:?}"),
        "GenerationId(00112233445566778899aabbccddeeff)"
    );
}
