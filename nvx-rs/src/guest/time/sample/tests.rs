use super::*;

fn measurement(epsilon: i64) -> BestSample {
    BestSample {
        theta: 1500,
        epsilon,
        host: HostClock::default(),
        frequency: 0,
        limited: false,
        test_hooks: false,
    }
}

#[test]
fn shared_selection_retries_invalid_records_and_retains_the_best_wide_sample() {
    let mut replies = [
        Err(AttemptError::Retry("malformed")),
        Ok(measurement(SAMPLE_BOUND_NS + 20)),
        Ok(measurement(SAMPLE_BOUND_NS + 10)),
    ]
    .into_iter();
    let (result, sample) =
        select_sample(3, SAMPLE_BOUND_NS, "empty", || replies.next().unwrap()).unwrap();
    assert_eq!(result, SampleResult::Wide);
    assert_eq!(sample.epsilon, SAMPLE_BOUND_NS + 10);
    assert!(replies.next().is_none());
}

#[test]
fn shared_selection_accepts_the_bound_and_never_hides_a_fatal_contract_error() {
    let mut attempts = 0;
    let selected = select_sample(3, SAMPLE_BOUND_NS, "empty", || {
        attempts += 1;
        Ok(measurement(SAMPLE_BOUND_NS))
    })
    .unwrap();
    assert_eq!(selected.0, SampleResult::Accepted);
    assert_eq!(attempts, 1);
    let mut attempts = 0;
    let failed = select_sample(3, SAMPLE_BOUND_NS, "empty", || {
        attempts += 1;
        if attempts == 1 {
            Ok(measurement(SAMPLE_BOUND_NS + 1))
        } else {
            Err(AttemptError::Fatal("generation or binding changed"))
        }
    });
    assert_eq!(failed.unwrap_err(), "generation or binding changed");
    assert_eq!(attempts, 2);
}

#[test]
fn shared_diagnostic_keeps_raw_fields_when_pairing_is_invalid() {
    let mut raw = Sample::parse(&sample()).unwrap();
    raw.counter = 1_000_000;
    let diagnostic = DiagnosticSample {
        sample: raw,
        reads: Reads {
            before: 0,
            after: 1,
            realtime_ns: 1_000_000_000,
            realtime_counter: 1,
            realtime_half_cycles: 1,
        },
        rates: Rates {
            counter_hz: 24_000_001,
            kernel_hz: 24_000_001,
            frequency: 0,
        },
        #[cfg(target_arch = "aarch64")]
        profile: ("fixture.cpu.v1".into(), [0x12; 32]),
    };
    let (line, result) = diagnostic.format();
    assert!(result.is_err());
    assert!(line.starts_with("flags=0x00 generation=0 status=synchronized utc_ns=1000000000 "));
    assert!(line.contains("correction_bound=unknown max_error_us=unknown"));
    assert!(!line.contains("theta_ns="));
    assert!(line.ends_with('\n'));
}

#[test]
fn kernel_report_only_survives_boot_then_plain_status_even_when_checks_pass() {
    let command_line = b"console=hvc1 nvx_time_abi=report-only";
    let mut state = crate::guest::time::state::TimeState::default();
    state.check_mut(Phase::Boot).unwrap().status = Some(crate::guest::time::state::CheckStatus::Ok);
    for command in ["boot", "status"] {
        let mode = report_only(command_line, false);
        assert!(mode, "{command}");
        let markers = state.status_markers(mode);
        assert!(markers.starts_with("NVX-TIME-REPORT: v=1 phase=boot status=ok"));
        assert!(!markers.contains("NVX-TIME-ABI:"));
    }
    assert!(!report_only(b"other=nvx_time_abi=report-only", false));
    assert!(!report_only(b"nvx_time_abi=enforce", false));
    assert!(report_only(b"nvx_time_abi=enforce", true));
}

/// A wire fixture, not a host clock, counter rate or CPU-profile identity.
pub(super) fn sample() -> [u8; SIZE] {
    let mut bytes = [0; SIZE];
    bytes[0] = 1;
    bytes[2] = 1;
    bytes[8..16].copy_from_slice(&1_000_000_000_u64.to_le_bytes());
    bytes[16..24].copy_from_slice(&1234_u64.to_le_bytes());
    bytes[24..28].copy_from_slice(&9000_u32.to_le_bytes());
    bytes[32..40].copy_from_slice(&(-42_i64).to_le_bytes());
    bytes[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
    bytes[44..48].copy_from_slice(&u32::MAX.to_le_bytes());
    bytes
}

#[test]
fn common_codec_keeps_real_counter_units_and_unknown_bounds() {
    let decoded = Sample::parse(&sample()).unwrap();
    assert_eq!(decoded.counter, 1234);
    assert_eq!(decoded.pairing_ns, 9000);
    assert_eq!(decoded.correction, -42);
    assert_eq!(decoded.correction_bound, None);
    assert_eq!(decoded.max_error_us, None);
    let mut bytes = sample();
    bytes[40..44].copy_from_slice(&17_u32.to_le_bytes());
    assert_eq!(Sample::parse(&bytes).unwrap().correction_bound, Some(17));
}

#[test]
fn common_codec_preserves_v1_validation_and_trailing_byte_behavior() {
    let bytes = sample();
    assert_eq!(Sample::parse(&bytes[..47]), Err(Error::Incomplete));
    let mut extended = bytes.to_vec();
    extended.push(0xff);
    assert_eq!(Sample::parse(&extended), Sample::parse(&bytes));
    for (offset, value, expected) in [
        (0, 2, Error::Version(2)),
        (
            1,
            1,
            Error::Flags {
                flags: 1,
                reserved: 0,
            },
        ),
        (2, 4, Error::Status(4)),
        (
            3,
            1,
            Error::Flags {
                flags: 0,
                reserved: 1,
            },
        ),
    ] {
        let mut invalid = bytes;
        invalid[offset] = value;
        assert_eq!(Sample::parse(&invalid), Err(expected));
    }

    for utc in [0, u64::MAX] {
        let mut invalid = bytes;
        invalid[8..16].copy_from_slice(&utc.to_le_bytes());
        assert_eq!(Sample::parse(&invalid), Err(Error::Utc(utc)));
    }
}

#[test]
fn arm_pairing_uses_full_frequency_and_keeps_the_existing_uncertainty_terms() {
    let reads = Reads {
        before: 999,
        after: 1001,
        realtime_ns: 1_000_020_000,
        realtime_counter: 1480,
        realtime_half_cycles: 12,
    };
    let rates = Rates {
        counter_hz: 24_000_000,
        kernel_hz: 24_000_000,
        frequency: 0,
    };
    assert_eq!(
        pair(1_000_000_000, 1000, 9000, &reads, rates).ok(),
        Some((0, 10540))
    );
    assert_eq!(guest_frequency(1234, 24_000_001, 24_000_001), (1234, false));
    assert_eq!(
        guest_frequency(500 * 65536 + 1, 24_000_001, 24_000_001),
        (500 * 65536, true)
    );
}

#[test]
fn shared_discipline_preserves_negative_steps_threshold_and_pll_constants() {
    assert_eq!(split_offset(-1), (-1, 999_999_999));
    assert_eq!(
        plan_correction(-128_000_000, 9000, true, 42, 17),
        ClockAdjustment::Step {
            seconds: -1,
            nanoseconds: 872_000_000,
        }
    );
    for (fast, time_constant) in [(true, 4), (false, 6)] {
        assert_eq!(
            plan_correction(127_999_999, 9001, fast, 42, 17),
            ClockAdjustment::Slew {
                offset_ns: 127_999_999,
                time_constant,
                hold: FrequencyHold {
                    scaled_ppm: 42,
                    maximum_error_us: 128_026,
                    estimated_error_us: 10,
                },
            }
        );
    }
    assert_eq!(frequency_to_ppb(-65536), -1000);
    assert_eq!(bound_to_ppb(1), 1);
}
