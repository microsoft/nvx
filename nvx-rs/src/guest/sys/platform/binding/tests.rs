use super::*;

/// A test-only identity, never an approved or published CPU profile.
fn identity() -> [u8; 96] {
    let mut bytes = [0; 96];
    bytes[..4].copy_from_slice(b"NVT\0");
    bytes[4] = 1;
    bytes[8..10].copy_from_slice(&1_u16.to_le_bytes());
    bytes[12..16].copy_from_slice(&8_u32.to_le_bytes());
    bytes[16..24].copy_from_slice(&24_000_000_u64.to_le_bytes());
    bytes[24..28].copy_from_slice(&20_u32.to_le_bytes());
    bytes[28..32].copy_from_slice(&15_u32.to_le_bytes());
    bytes[32..47].copy_from_slice(b"test.profile.v1");
    bytes[64..].fill(0x55);
    bytes
}

#[test]
fn exact_development_record_decodes_without_approving_a_cpu_profile() {
    let binding = TimeBinding::parse(&identity()).unwrap();
    assert_eq!(binding.capacity, 8);
    assert_eq!(binding.counter_hz, 24_000_000);
    assert_eq!(binding.profile_id, "test.profile.v1");
    assert_eq!(binding.profile_sha256, [0x55; 32]);
}

#[test]
fn truncated_extended_or_mismatched_identity_is_rejected() {
    let valid = identity();
    for size in [0, 4, 8, 32, 64, 95] {
        assert!(TimeBinding::parse(&valid[..size]).is_err());
    }
    assert!(TimeBinding::parse(&[0; 97]).is_err());
    for offset in [0, 4, 5, 6, 8, 10, 12, 24, 28, 32, 47] {
        let mut bytes = valid;
        bytes[offset] = 0xff;
        assert!(TimeBinding::parse(&bytes).is_err(), "offset {offset}");
    }
    for frequency in [0, u64::from(u32::MAX) + 1] {
        let mut bytes = valid;
        bytes[16..24].copy_from_slice(&frequency.to_le_bytes());
        assert!(TimeBinding::parse(&bytes).is_err());
    }
}
