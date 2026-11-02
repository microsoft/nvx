use super::*;

#[test]
fn zeroize_overwrites_every_byte_of_its_slice_alone() {
    let mut bytes: [u8; 67] = std::array::from_fn(|index| index as u8 | 1);
    zeroize(&mut bytes[1..66]);
    assert_eq!((bytes[0], bytes[66]), (1, 67));
    assert!(bytes[1..66].iter().all(|&byte| byte == 0));
    zeroize(&mut []);
}
