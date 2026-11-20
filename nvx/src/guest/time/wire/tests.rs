use super::*;

/// A valid time sample of generation 4, as OpenVMM's `TimeSample::encode` writes it.
pub(crate) fn valid_sample() -> [u8; SAMPLE_SIZE] {
    let mut bytes = [0; SAMPLE_SIZE];
    bytes[..4].copy_from_slice(&[1, 8, 1, 0]);
    bytes[4..8].copy_from_slice(&4_u32.to_le_bytes());
    bytes[8..16].copy_from_slice(&1_790_841_600_123_456_789_u64.to_le_bytes());
    bytes[16..24].copy_from_slice(&123_456_789_u64.to_le_bytes());
    bytes[24..28].copy_from_slice(&640_u32.to_le_bytes());
    bytes[28..32].copy_from_slice(&3_u32.to_le_bytes());
    bytes[32..40].copy_from_slice(&(-6_552_945_i64).to_le_bytes());
    bytes[40..44].copy_from_slice(&66_u32.to_le_bytes());
    bytes[44..48].copy_from_slice(&u32::MAX.to_le_bytes());
    bytes
}

#[test]
fn samples_decode_every_field() {
    assert_eq!(
        TimeSample::parse(&valid_sample()),
        Ok(TimeSample {
            flags: 8,
            generation: 4,
            utc_ns: 1_790_841_600_123_456_789,
            tsc: 123_456_789,
            pairing_ns: 640,
            host: HostClock {
                status: HostClockStatus::Synchronized,
                disruption: 3,
                correction: -6_552_945,
                correction_bound: Some(66),
                max_error_us: None,
            },
        })
    );
    assert_eq!(
        TimeSample::parse(&valid_sample()[..47]),
        Err(TimeSampleError::Incomplete)
    );
}

#[test]
fn samples_check_their_version_flags_status_and_time() {
    let mutated = |offset: usize, bytes: &[u8]| {
        let mut sample = valid_sample();
        sample[offset..offset + bytes.len()].copy_from_slice(bytes);
        TimeSample::parse(&sample).unwrap_err().to_string()
    };
    assert_eq!(mutated(0, &[2]), "time sample version is 2");
    assert_eq!(mutated(1, &[1]), "time sample flags 0x01 or reserved 0x00");
    assert_eq!(mutated(3, &[1]), "time sample flags 0x08 or reserved 0x01");
    assert_eq!(mutated(2, &[4]), "time sample host status is 4");
    assert_eq!(mutated(8, &[0; 8]), "time sample UTC 0 is invalid");
    assert_eq!(
        mutated(8, &u64::MAX.to_le_bytes()),
        format!("time sample UTC {} is invalid", u64::MAX)
    );
}

#[test]
fn the_test_hook_flag_reports_host_overrides() {
    let mut sample = valid_sample();
    assert!(TimeSample::parse(&sample).unwrap().test_hooks());
    sample[1] = 0;
    assert!(!TimeSample::parse(&sample).unwrap().test_hooks());
}
