use proptest::prelude::*;

use super::*;
use crate::protocol::cmdline::tests::model;

/// A valid packet header without memory ranges, of generation 1.
fn valid_header() -> [u8; PACKET_HEADER_SIZE] {
    let mut bytes = [0; PACKET_HEADER_SIZE];
    bytes[..4].copy_from_slice(b"OVR\x01");
    bytes[8..12].copy_from_slice(&1_u32.to_le_bytes());
    bytes
}

/// The body of a packet without memory ranges or network settings: `entropy`, and a DNS server
/// of zeros.
fn plain_body(entropy: &[u8]) -> Vec<u8> {
    [entropy, &[0; PACKET_DNS_SIZE]].concat()
}

/// Reads the activation record of a packet that has none.
fn unread(_: usize) -> Vec<u8> {
    unreachable!("the packet has no activation record")
}

#[test]
fn packets_decode_their_header_then_their_body() {
    let header = PacketHeader::parse(&valid_header()).unwrap();
    assert_eq!((header.generation, header.downtime_ns), (1, 0));
    assert_eq!(header.body_size(), PACKET_ENTROPY_SIZE + PACKET_DNS_SIZE);
    let entropy: Vec<u8> = (0..64).collect();
    let body = plain_body(&entropy);
    let packet = RestorePacket::parse(header, &body, unread).unwrap();
    assert_eq!(packet.entropy.bytes().as_slice(), entropy);
    assert_eq!(packet.generation_id(), &entropy[..GENERATION_ID_SIZE]);
    assert_eq!(
        RestorePacket::parse(header, &body[1..], unread).unwrap_err(),
        PacketError::IncompleteBody
    );
    assert_eq!(
        PacketHeader::parse(&valid_header()[..23]),
        Err(PacketError::IncompleteHeader)
    );
    // A packet of another version is refused.
    let mut bytes = valid_header();
    bytes[3] = 5;
    assert_eq!(PacketHeader::parse(&bytes), Err(PacketError::Version(5)));
}

#[test]
fn packet_errors_name_the_invalid_field() {
    let mut bytes = valid_header();
    bytes[2] = b'X';
    let error = PacketHeader::parse(&bytes).unwrap_err();
    assert_eq!(error.to_string(), "restore packet magic is 4f5658");
    let mut bytes = valid_header();
    bytes[4] = 0x02;
    bytes[6] = 1;
    let header = PacketHeader::parse(&bytes).unwrap();
    let mut body = [0; PACKET_RANGE_SIZE + PACKET_ENTROPY_SIZE + PACKET_DNS_SIZE];
    body[..8].copy_from_slice(&u64::MAX.to_le_bytes());
    body[8..16].copy_from_slice(&1_u64.to_le_bytes());
    let error = RestorePacket::parse(header, &body, unread).unwrap_err();
    assert_eq!(error.to_string(), "restore memory range 0 is invalid");
}

#[test]
fn packets_accept_a_downtime_of_up_to_365_days() {
    let with_downtime = |downtime_ns: u64| {
        let mut bytes = valid_header();
        bytes[16..24].copy_from_slice(&downtime_ns.to_le_bytes());
        PacketHeader::parse(&bytes)
    };
    // 365 days of 86,400 seconds, the bound included.
    let limit = 31_536_000_000_000_000_u64;
    assert_eq!(with_downtime(limit).unwrap().downtime_ns, limit);
    let error = with_downtime(limit + 1).unwrap_err();
    assert_eq!(error, PacketError::Downtime(limit + 1));
    assert_eq!(
        error.to_string(),
        "restore packet downtime 31536000000000001 ns exceeds the 365-day limit"
    );
    assert_eq!(
        with_downtime(u64::MAX).unwrap_err(),
        PacketError::Downtime(u64::MAX)
    );
}

/// A valid time sample of generation 4, as OpenVMM's `TimeSample::encode` writes it.
fn valid_sample() -> [u8; SAMPLE_SIZE] {
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

/// Returns a valid packet header that carries network settings, without memory ranges, of
/// generation 1, with the network byte `network`.
fn network_header(network: u8) -> [u8; PACKET_HEADER_SIZE] {
    let mut bytes = valid_header();
    bytes[4] = PACKET_NETWORK;
    bytes[7] = network;
    bytes
}

#[test]
fn network_packets_carry_their_network_settings() {
    let entropy: Vec<u8> = (0..64).collect();
    let ipv6: std::net::Ipv6Addr = "fd00::a00:1".parse().unwrap();
    for (network, dns_server, expected) in [
        (
            0x05,
            [10, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            RestoreNetwork::new(Some(IpAddr::from([10, 0, 0, 1])), true),
        ),
        (
            0x02,
            ipv6.octets(),
            RestoreNetwork::new(Some(IpAddr::V6(ipv6)), false),
        ),
        (0x04, [0; 16], RestoreNetwork::new(None, true)),
        (0x00, [0; 16], RestoreNetwork::new(None, false)),
    ] {
        let header = PacketHeader::parse(&network_header(network)).unwrap();
        assert_eq!(header.network, Some(network));
        assert_eq!(header.network_repair(), network != 0);
        assert_eq!(header.body_size(), PACKET_ENTROPY_SIZE + PACKET_DNS_SIZE);
        let body = [entropy.as_slice(), &dns_server].concat();
        let packet = RestorePacket::parse(header, &body, unread).unwrap();
        assert_eq!(packet.entropy.bytes().as_slice(), entropy);
        assert_eq!(packet.network, Some(expected), "{network:#04x}");
        assert_eq!(
            RestorePacket::parse(header, &body[1..], unread).unwrap_err(),
            PacketError::IncompleteBody
        );
    }

    // Without the network flag, the network byte and the DNS server are zero, and the packet
    // carries no network settings.
    let header = PacketHeader::parse(&valid_header()).unwrap();
    assert_eq!(header.network, None);
    assert!(!header.network_repair());
    let packet = RestorePacket::parse(header, &plain_body(&entropy), unread).unwrap();
    assert_eq!(packet.network, None);
}

#[test]
fn packets_refuse_malformed_network_settings() {
    for (network, error) in [
        (0x08, "restore packet network byte 0x08 is invalid"),
        (0x03, "restore packet network byte 0x03 is invalid"),
    ] {
        let error_text = PacketHeader::parse(&network_header(network))
            .unwrap_err()
            .to_string();
        assert_eq!(error_text, error);
    }
    let mut bytes = valid_header();
    bytes[7] = 1;
    assert_eq!(
        PacketHeader::parse(&bytes),
        Err(PacketError::ReservedByte(1))
    );
    assert_eq!(
        PacketError::ReservedByte(1).to_string(),
        "restore packet network byte is 1 without network settings"
    );
    bytes[3] = 7;
    assert_eq!(PacketHeader::parse(&bytes), Err(PacketError::Version(7)));
    // Without the network flag, a DNS server is refused.
    let header = PacketHeader::parse(&valid_header()).unwrap();
    let mut body = plain_body(&[0; PACKET_ENTROPY_SIZE]);
    body[PACKET_ENTROPY_SIZE] = 10;
    assert_eq!(
        RestorePacket::parse(header, &body, unread).unwrap_err(),
        PacketError::DnsServer
    );

    let ipv4 = [10, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let mut padded = ipv4;
    padded[15] = 1;
    for (network, dns_server) in [
        // A nonzero byte after an IPv4 server.
        (0x01, padded),
        // A server that the network byte does not name.
        (0x00, ipv4),
        // Unspecified servers.
        (0x01, [0; 16]),
        (0x02, [0; 16]),
    ] {
        let header = PacketHeader::parse(&network_header(network)).unwrap();
        let body = [&[0; PACKET_ENTROPY_SIZE][..], &dns_server].concat();
        let error = RestorePacket::parse(header, &body, unread).unwrap_err();
        assert_eq!(error, PacketError::DnsServer, "{network:#04x}");
        assert_eq!(error.to_string(), "restore packet DNS server is invalid");
    }
}

/// Returns a valid packet header with an activation record, without memory ranges, of
/// generation 1.
fn activation_header() -> [u8; PACKET_HEADER_SIZE] {
    let mut bytes = valid_header();
    bytes[4] = PACKET_ACTIVATION | PACKET_ACK_REQUIRED;
    bytes
}

#[test]
fn packets_end_with_their_activation_record() {
    let header = PacketHeader::parse(&activation_header()).unwrap();
    assert!(header.activation() && header.acknowledgement_required());
    // The body ends with the record's length, so the record starts on a four-byte boundary.
    assert_eq!(
        header.body_size(),
        PACKET_ENTROPY_SIZE + PACKET_DNS_SIZE + PACKET_ACTIVATION_LENGTH_SIZE
    );
    assert_eq!((PACKET_HEADER_SIZE + header.body_size()) % 4, 0);
    let entropy: Vec<u8> = (0..64).collect();
    for length in [1, 3, 602, PACKET_ACTIVATION_MAX] {
        let record: Vec<u8> = (0..length).map(|index| index as u8 ^ 0x5a).collect();
        let body = [plain_body(&entropy), (length as u32).to_le_bytes().to_vec()].concat();
        let mut requested = None;
        let packet = RestorePacket::parse(header, &body, |size| {
            requested = Some(size);
            record.clone()
        })
        .unwrap();
        assert_eq!(requested, Some(length));
        assert_eq!(packet.activation.as_deref(), Some(record.as_slice()));
        assert_eq!(packet.entropy.bytes().as_slice(), entropy);
    }
    // A packet without the flag has none, and reads none.
    let header = PacketHeader::parse(&valid_header()).unwrap();
    assert!(!header.activation());
    let packet = RestorePacket::parse(header, &plain_body(&entropy), unread).unwrap();
    assert_eq!(packet.activation, None);
}

#[test]
fn packets_refuse_activation_records_beyond_the_bounds() {
    let header = PacketHeader::parse(&activation_header()).unwrap();
    for length in [0, PACKET_ACTIVATION_MAX as u32 + 1, u32::MAX] {
        let body = [
            plain_body(&[0; PACKET_ENTROPY_SIZE]),
            length.to_le_bytes().to_vec(),
        ]
        .concat();
        let error = RestorePacket::parse(header, &body, unread).unwrap_err();
        assert_eq!(error, PacketError::ActivationLength(length));
        assert_eq!(
            error.to_string(),
            format!("restore packet activation record length {length} is not 1 to 1024")
        );
    }
    // A body that ends within the record's length.
    let body = plain_body(&[0; PACKET_ENTROPY_SIZE]);
    assert_eq!(
        RestorePacket::parse(header, &[body.as_slice(), &[1, 0]].concat(), unread).unwrap_err(),
        PacketError::IncompleteBody
    );
    // The bits after the activation flag, which no version defines.
    for flags in [0x40, 0x80] {
        let mut bytes = valid_header();
        bytes[4] = flags;
        assert_eq!(
            PacketHeader::parse(&bytes),
            Err(PacketError::ReservedFlags(flags))
        );
    }
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

#[test]
fn the_host_error_counts_only_while_a_time_service_synchronizes_the_host() {
    let host = |status, max_error_us| HostClock {
        status,
        max_error_us,
        ..HostClock::default()
    };
    assert_eq!(
        host(HostClockStatus::Synchronized, Some(12_500)).error_us(),
        12_500
    );
    assert_eq!(host(HostClockStatus::Synchronized, None).error_us(), 0);
    assert_eq!(
        host(HostClockStatus::FreeRunning, Some(16_000_000)).error_us(),
        0
    );
    assert_eq!(host(HostClockStatus::Unknown, Some(5)).error_us(), 0);
}

/// A sample of guest TSC 1,000,000 at host UTC 10^18, paired within 500 ns.
fn paired_sample(tsc: u64) -> TimeSample {
    TimeSample {
        utc_ns: 1_000_000_000_000_000_000,
        tsc,
        pairing_ns: 500,
        ..TimeSample::parse(&valid_sample()).unwrap()
    }
}

/// The guest's reads around the latch of [`paired_sample`]: its TSC 1,000 cycles before and
/// 2,000 after the sample's, and its realtime clock 4,000 cycles after, within 40 cycles.
const READS: GuestReads = GuestReads {
    before: 999_000,
    after: 1_002_000,
    realtime_ns: 999_999_999_999_000_000,
    realtime_tsc: 1_004_000,
    realtime_half_cycles: 40,
};

/// A 2 GHz TSC, without a frequency correction.
const RATES: ClockRates = ClockRates {
    tsc_hz: 2_000_000_000,
    frequency: 0,
};

#[test]
fn a_pairing_extrapolates_the_realtime_clock_back_to_the_sample() {
    // The realtime read is 4,000 cycles, 2 µs, after the sample's TSC, so the guest's clock
    // was 999_999_999_998_998_000 then, 1_002_000 ns behind host UTC. The uncertainty adds
    // 1 µs of cross-CPU skew, 20 ns of the realtime read, and 4 ns of a slew over the
    // 2 µs, to the host's 500 ns.
    assert_eq!(
        pair_sample(&paired_sample(1_000_000), &READS, RATES),
        Ok((1_002_000, 1_524))
    );
    // The kernel counts the TSC at the declared rate to the kHz, with its frequency
    // correction.
    let late = GuestReads {
        realtime_tsc: 1_000_000 + 2_000_000_000,
        ..READS
    };
    let (theta, _) = pair_sample(
        &paired_sample(1_000_000),
        &late,
        ClockRates {
            tsc_hz: 2_000_000_999,
            frequency: 0,
        },
    )
    .unwrap();
    assert_eq!(theta, 1_000_000 + 1_000_000_000);
    let (theta, epsilon) = pair_sample(
        &paired_sample(1_000_000),
        &GuestReads {
            realtime_tsc: 1_000_000 + 20_000_000,
            ..READS
        },
        ClockRates {
            frequency: 6_553_600,
            ..RATES
        },
    )
    .unwrap();
    assert_eq!(theta, 1_000_000 + 10_001_000);
    assert_eq!(epsilon, 500 + 1_000 + 20 + 20_002);
}

#[test]
fn a_sample_outside_the_guest_reads_is_refused() {
    // The tolerance is 1.5 µs, the pairing and the skew bound: 3,000 cycles at 2 GHz.
    for tsc in [996_000, 1_005_000] {
        assert!(
            pair_sample(&paired_sample(tsc), &READS, RATES).is_ok(),
            "{tsc}"
        );
    }
    for tsc in [995_999, 1_005_001, 0, u64::MAX] {
        let error = pair_sample(&paired_sample(tsc), &READS, RATES).unwrap_err();
        assert_eq!(
            error,
            TimeSampleError::Tsc {
                tsc,
                before: 999_000,
                after: 1_002_000,
            }
        );
        assert_eq!(
            error.to_string(),
            format!("time sample TSC {tsc} is outside the guest's reads 999000 to 1002000")
        );
    }
}

#[test]
fn the_guest_frequency_applies_the_host_correction_to_the_kernel_rate() {
    // A declared rate of whole kHz: the correction itself.
    assert_eq!(
        guest_frequency(13_107_200, 2_100_000_000),
        (13_107_200, false)
    );
    assert_eq!(
        guest_frequency(-6_552_945, 2_000_000_000),
        (-6_552_945, false)
    );
    // The kernel's rate, to the kHz, is 123 Hz slow: about 0.056 ppm.
    assert_eq!(guest_frequency(0, 2_194_843_123), (-3_673, false));
    assert_eq!(
        guest_frequency(13_107_200, 2_194_843_123),
        (13_103_527, false)
    );
    // Linux's limit of 500 ppm holds the frequency, and the call reports it.
    assert_eq!(
        guest_frequency(39_321_600, 2_100_000_000),
        (32_768_000, true)
    );
    assert_eq!(
        guest_frequency(-39_321_600, 2_100_000_000),
        (-32_768_000, true)
    );
    assert_eq!(guest_frequency(i64::MAX, 2_100_000_000), (32_768_000, true));
}

#[test]
fn rates_convert_to_parts_per_billion() {
    assert_eq!(frequency_to_ppb(13_107_200), 200_000);
    assert_eq!(frequency_to_ppb(-6_552_945), -99_990);
    assert_eq!(frequency_to_ppb(-3_673), -56);
    assert_eq!(frequency_to_ppb(i64::MIN), -(1_i64 << 47) * 1000);
    assert_eq!(bound_to_ppb(66), 2);
    assert_eq!(bound_to_ppb(13_107_200), 200_000);
    assert_eq!(bound_to_ppb(0), 0);
}

#[test]
fn generation_ids_are_exact_hexadecimal() {
    assert_eq!(parse_hex::<2>(b"0aFf"), Some([0x0a, 0xff]));
    assert_eq!(parse_hex::<2>(b"0aF"), None);
    assert_eq!(parse_hex::<2>(b"+aff"), None);
    assert_eq!(parse_hex::<2>(b"0agf"), None);
    assert_eq!(format_hex(&[0x0a, 0xff]), "0aff");
}

#[test]
fn correction_boundary_matches_the_abi() {
    assert!(matches!(
        plan_correction(128_000_000, 10, true, 0, 0),
        ClockAdjustment::Step { .. }
    ));
    assert!(matches!(
        plan_correction(127_999_999, 10, true, 0, 0),
        ClockAdjustment::Slew { .. }
    ));
    assert_eq!(
        plan_correction(-1_128_000_000, 0, false, 0, 0),
        ClockAdjustment::Step {
            seconds: -2,
            nanoseconds: 872_000_000,
        }
    );
}

#[test]
fn a_slew_holds_the_frequency_with_the_host_error() {
    assert_eq!(
        plan_correction(-1_500, 2_100, false, 13_107_200, 12_500),
        ClockAdjustment::Slew {
            offset_ns: -1_500,
            time_constant: 6,
            hold: FrequencyHold {
                scaled_ppm: 13_107_200,
                maximum_error_us: 4 + 12_500,
                estimated_error_us: 3,
            },
        }
    );
    assert_eq!(
        frequency_hold(0, 524, -3_673, 0),
        FrequencyHold {
            scaled_ppm: -3_673,
            maximum_error_us: 1,
            estimated_error_us: 1,
        }
    );
}

#[test]
fn offsets_split_toward_negative_infinity() {
    assert_eq!(split_offset(-1), (-1, 999_999_999));
    assert_eq!(split_offset(1_500_000_000), (1, 500_000_000));
    assert_eq!(split_offset(i64::MIN), (-9_223_372_037, 145_224_192));
}

#[test]
fn extreme_clock_arithmetic_neither_wraps_nor_panics() {
    assert_eq!(
        plan_correction(i64::MIN, 0, true, 0, 0),
        ClockAdjustment::Step {
            seconds: -9_223_372_037,
            nanoseconds: 145_224_192,
        }
    );
    let slew = plan_correction(-5, i64::MAX, true, 7, i64::MAX);
    assert_eq!(
        slew,
        ClockAdjustment::Slew {
            offset_ns: -5,
            time_constant: 4,
            hold: FrequencyHold {
                scaled_ppm: 7,
                maximum_error_us: i64::MAX,
                estimated_error_us: i64::MAX / 1000,
            },
        }
    );
    assert_eq!((slew.modes(), slew.status()), (0x203f, 0x2081));
    let extreme = TimeSample {
        utc_ns: i64::MAX as u64,
        pairing_ns: u32::MAX,
        ..paired_sample(u64::MAX)
    };
    let reads = GuestReads {
        before: u64::MAX,
        after: u64::MAX,
        realtime_ns: i64::MIN,
        realtime_tsc: 0,
        realtime_half_cycles: u64::MAX,
    };
    let (theta, epsilon) = pair_sample(
        &extreme,
        &reads,
        ClockRates {
            tsc_hz: u64::MAX,
            frequency: i64::MAX,
        },
    )
    .unwrap();
    assert_eq!(theta, i64::MAX);
    assert!(epsilon > i64::from(u32::MAX), "{epsilon}");
    let (theta, epsilon) = pair_sample(
        &extreme,
        &reads,
        ClockRates {
            tsc_hz: 0,
            frequency: i64::MIN,
        },
    )
    .unwrap();
    assert_eq!((theta, epsilon), (i64::MIN, i64::MAX));
    assert_eq!(guest_frequency(i64::MIN, u64::MAX), (-32_768_000, true));
    assert_eq!(guest_frequency(0, 0), (-32_768_000, true));
}

#[test]
fn watcher_ignores_its_own_event() {
    assert_eq!(
        match_watch_rule(b"unchecked MSR access error"),
        Some(ViolationCode::UncheckedMsr)
    );
    assert_eq!(
        match_watch_rule(b"NVX-TIME-ABI-VIOLATION: unchecked MSR access error"),
        None
    );
}

#[test]
fn watcher_rules_match_linux_6_18_messages() {
    let messages = [
        (
            "tsc: Marking TSC unstable due to clocksource watchdog",
            Some(ViolationCode::TscUnstable),
        ),
        (
            "clocksource: timekeeping watchdog on CPU0: Marking clocksource \
                 'tsc-early' as unstable because the skew is too large:",
            Some(ViolationCode::ClocksourceUnstable),
        ),
        (
            "clocksource: Switched to clocksource refined-jiffies",
            Some(ViolationCode::ClocksourceSwitch),
        ),
        (
            "clocksource:         CPUs 1 ahead of CPU 0 for clocksource tsc.",
            Some(ViolationCode::ClocksourceSkew),
        ),
        (
            "clocksource:         CPUs 2-3 behind CPU 0 for clocksource tsc.",
            Some(ViolationCode::ClocksourceSkew),
        ),
        (
            "TSC synchronization [CPU#0 -> CPU#1]:",
            Some(ViolationCode::TscWarp),
        ),
        (
            "Measured 45 cycles TSC warp between CPUs, turning off TSC clock.",
            Some(ViolationCode::TscWarp),
        ),
        (
            "TSC warped randomly between CPUs",
            Some(ViolationCode::TscWarp),
        ),
        (
            "[Firmware Bug]: TSC ADJUST differs: CPU1 0 --> 100. Restoring",
            Some(ViolationCode::TscAdjust),
        ),
        (
            "rcu: INFO: rcu_preempt detected stalls on CPUs/tasks:",
            Some(ViolationCode::RcuStall),
        ),
        (
            "rcu: INFO: rcu_preempt self-detected stall on CPU",
            Some(ViolationCode::RcuStall),
        ),
        (
            "rcu: INFO: rcu_preempt detected expedited stalls on CPUs/tasks: { 1-... }",
            Some(ViolationCode::RcuStall),
        ),
        (
            "rcu: rcu_preempt kthread starved for 2603 jiffies! g345 f0x0",
            Some(ViolationCode::RcuStarved),
        ),
        (
            "rcu: rcu_preempt kthread timer wakeup didn't happen for 2602 jiffies! \
                 g345 f0x0",
            Some(ViolationCode::RcuStarved),
        ),
        (
            "watchdog: BUG: soft lockup - CPU#0 stuck for 22s! [sh:123]",
            Some(ViolationCode::SoftLockup),
        ),
        (
            "watchdog: Watchdog detected hard LOCKUP on cpu 1",
            Some(ViolationCode::HardLockup),
        ),
        (
            "INFO: task kworker/0:1:12 blocked for more than 120 seconds.",
            Some(ViolationCode::HungTask),
        ),
        (
            "unchecked MSR access error: RDMSR from 0x1ad at rIP: \
                 0xffffffff816f11ee (__rdmsr_on_cpu+0x1e/0x30)",
            Some(ViolationCode::UncheckedMsr),
        ),
        (
            "[Firmware Bug]: CPU   1: APIC ID mismatch. CPUID: 0x0000 APIC: 0x0001",
            Some(ViolationCode::ApicIdMismatch),
        ),
        (
            "clocksource: tsc: mask: 0xffffffffffffffff max_cycles: 0x1fa3",
            None,
        ),
        ("Hyper-V: LAPIC Timer Frequency: 0x1e8480", None),
        ("rcu: Hierarchical RCU implementation.", None),
        (
            "NVX-TIME-ABI-VIOLATION: v=1 code=G_UNCHECKED_MSR \
                 detail=\"unchecked MSR access error\"",
            None,
        ),
    ];
    for (message, expected) in messages {
        assert_eq!(match_watch_rule(message.as_bytes()), expected, "{message}");
    }
}

#[test]
fn boot_log_reads_syslog_lines() {
    let log = scan_boot_log(
        b"<6>[    0.000000] Hypervisor detected: Microsoft Hyper-V\n\
              <6>[    0.000000] Hyper-V: privilege flags low 0x8860, high 0x0\n\
              <6>[    0.000000] Hyper-V: LAPIC Timer Frequency: 0x1e8480\n\
              <6>[    1.000000] clocksource: Switched to clocksource tsc\n\
              <4>[    1.500000] x86/mm: Found insecure W+X mapping at address 0x1\n",
        true,
    );
    assert_eq!(log.verdict(), Ok(()));
    assert_eq!(
        log.wx.as_deref(),
        Some(b"x86/mm: Found insecure W+X mapping at address 0x1".as_slice())
    );
    let log = scan_boot_log(b"clocksource: Switched to clocksource hpet\n", false);
    assert_eq!(
        log.verdict(),
        Err(b"no 'Hypervisor detected: Microsoft Hyper-V'".to_vec())
    );
}

#[test]
fn boot_log_requires_the_identity_and_a_final_switch_to_tsc() {
    const BOOT_LOG: &str = "Hypervisor detected: Microsoft Hyper-V\n\
            Hyper-V: privilege flags low 0x8860, high 0x0, ext 0x0, hints 0x0, misc 0x100\n\
            Hyper-V: LAPIC Timer Frequency: 0x1e8480\n\
            tsc: Detected 2194.843 MHz processor\n\
            clocksource: Switched to clocksource tsc-early\n\
            clocksource: Switched to clocksource tsc\n";
    assert_eq!(scan_boot_log(BOOT_LOG.as_bytes(), false).verdict(), Ok(()));
    assert_eq!(
        scan_boot_log(BOOT_LOG.replace("0x1e8480", "0x989680").as_bytes(), false).verdict(),
        Ok(())
    );
    for (name, log) in [
        (
            "missing identity",
            BOOT_LOG.lines().skip(1).collect::<Vec<_>>().join("\n"),
        ),
        ("wrong LAPIC rate", BOOT_LOG.replace("0x1e8480", "0x1e8481")),
        (
            "last switch",
            format!("{BOOT_LOG}clocksource: Switched to clocksource refined-jiffies\n"),
        ),
        (
            "kvm-clock",
            format!("{BOOT_LOG}kvm-clock: Using msrs 4b564d01\n"),
        ),
        (
            "refinement",
            format!("{BOOT_LOG}tsc: Refined TSC clocksource calibration: 2194.843 MHz\n"),
        ),
        (
            "lapic token",
            format!("{BOOT_LOG}APIC timer: using supplied frequency 200000000 Hz\n"),
        ),
        (
            "watcher record",
            format!("{BOOT_LOG}tsc: Marking TSC unstable due to clocksource watchdog\n"),
        ),
    ] {
        assert!(
            scan_boot_log(log.as_bytes(), false).verdict().is_err(),
            "{name}"
        );
    }
}

#[test]
fn boot_log_reads_syslog_read_all_lines() {
    let lines = concat!(
        "<6>[    0.000000] Hypervisor detected: Microsoft Hyper-V\n",
        "<6>[    0.123456] [drm] a bracketed message is kept\n",
        "<6>[    1.007919] Hyper-V: privilege flags low 0x8860, high 0x0, ",
        "ext 0x0, hints 0x0, misc 0x100\n",
        "<6>[    2.015838] Hyper-V: LAPIC Timer Frequency: 0x1e8480\n",
        "<6>[    3.023757] tsc: Detected 2194.843 MHz processor\n",
        "<6>[    4.031676] clocksource: Switched to clocksource tsc-early\n",
        "<6>[    5.039595] clocksource: Switched to clocksource tsc\n"
    );
    assert_eq!(scan_boot_log(lines.as_bytes(), true).verdict(), Ok(()));
    let own = concat!(
        "<2>[    3.000000] NVX-TIME-REPORT-VIOLATION: v=1 ",
        "code=G_CONFORMANCE_C4 detail=\"forbidden record: kvm-clock: Using msrs\"\n"
    );
    assert_eq!(
        scan_boot_log(format!("{lines}{own}").as_bytes(), true).verdict(),
        Ok(())
    );
    assert!(
        scan_boot_log(
            format!("{lines}<6>[    4.000000] kvm-clock: Using msrs\n").as_bytes(),
            true
        )
        .verdict()
        .unwrap_err()
        .starts_with(b"forbidden record: kvm-clock")
    );
    assert!(
        scan_boot_log(
            lines
                .lines()
                .skip(2)
                .collect::<Vec<_>>()
                .join("\n")
                .as_bytes(),
            true
        )
        .verdict()
        .unwrap_err()
        .starts_with(b"no 'Hypervisor")
    );
}

#[test]
fn boot_log_reports_a_kernel_wx_mapping_apart_from_c4() {
    const BOOT_LOG: &str = "<6>[    0.100000] Hypervisor detected: Microsoft Hyper-V\n\
            <6>[    0.100000] Hyper-V: privilege flags low 0x8860, high 0x0, ext 0x0\n\
            <6>[    0.100000] Hyper-V: LAPIC Timer Frequency: 0x1e8480\n\
            <6>[    0.100000] clocksource: Switched to clocksource tsc\n";
    const WX: &str = "<4>[    0.412345] ------------[ cut here ]------------\n\
            <4>[    0.412346] x86/mm: Found insecure W+X mapping at address 0xffffffffc0000000\n\
            <4>[    0.412400] WARNING: CPU: 0 PID: 1 at \
            arch/x86/mm/dump_pagetables.c:246 note_wx+0x5a/0x70\n\
            <6>[    0.412500] x86/mm: Checked W+X mappings: FAILED, 4 W+X pages found.\n";
    let log = scan_boot_log(format!("{WX}{BOOT_LOG}").as_bytes(), true);
    assert_eq!(log.verdict(), Ok(()));
    assert_eq!(
        log.wx.as_deref(),
        Some(b"x86/mm: Found insecure W+X mapping at address 0xffffffffc0000000".as_slice())
    );
    let passed = scan_boot_log(
        format!(
            "{BOOT_LOG}<6>[    0.412500] x86/mm: Checked W+X mappings: passed, \
                 no W+X pages found.\n"
        )
        .as_bytes(),
        true,
    );
    assert_eq!((passed.verdict(), passed.wx), (Ok(()), None));
    let missing = scan_boot_log(
        format!(
            "{WX}<6>[    0.100000] Hyper-V: privilege flags low 0x8860, high 0x0\n\
                 <6>[    0.100000] Hyper-V: LAPIC Timer Frequency: 0x1e8480\n\
                 <6>[    0.100000] clocksource: Switched to clocksource tsc\n"
        )
        .as_bytes(),
        true,
    );
    assert!(
        missing
            .verdict()
            .unwrap_err()
            .starts_with(b"no 'Hypervisor")
    );
    assert!(missing.wx.is_some());
    let own = concat!(
        "<2>[    3.000000] NVX-TIME-REPORT-VIOLATION: v=1 code=G_KERNEL_WX ",
        "detail=\"x86/mm: Found insecure W+X mapping at address 0x0\"\n"
    );
    let ignored = scan_boot_log(format!("{BOOT_LOG}{own}").as_bytes(), true);
    assert_eq!((ignored.verdict(), ignored.wx), (Ok(()), None));
}

#[test]
fn timer_list_requires_one_shot_lapic_ticks() {
    let list = b"Tick Device: mode:     1\n\
                     Broadcast device\n\
                     Clock Event Device: <NULL>\n\
                     \n\
                     Tick Device: mode:     1\n\
                     Per CPU device: 0\n\
                     Clock Event Device: lapic\n \
                     mode:           3\n \
                     event_handler:  hrtimer_interrupt\n";
    assert_eq!(timer_list_verdict(list, CpuMask::EMPTY.with(0)), Ok(()));
    assert_eq!(
        timer_list_verdict(list, CpuMask::EMPTY.with(1)),
        Err("cpu 1 tick device missing handler none tick mode -1 state -1".to_owned())
    );
}

#[test]
fn timer_list_covers_two_cpu_linux_6_18_variants() {
    fn cpu(cpu: usize, device: &str, state: i32, handler: &str, mode: i32) -> String {
        format!(
            "Tick Device: mode:     {mode}\n\
                 Per CPU device: {cpu}\n\
                 Clock Event Device: {device}\n\
                  max_delta_ns:   1374389534634\n\
                  min_delta_ns:   1000\n\
                  mult:           6710886\n\
                  shift:          26\n\
                  mode:           {state}\n\
                  next_event:     1342000000 nsecs\n\
                  set_next_event: lapic_next_event\n\
                  shutdown:       lapic_timer_shutdown\n\
                  periodic:       lapic_timer_set_periodic\n\
                  oneshot:        lapic_timer_set_oneshot\n\
                  oneshot stopped: lapic_timer_shutdown\n\
                  event_handler:  {handler}\n\
                  retries:        0\n\n"
        )
    }
    fn list(broadcast: &str, second: Option<(&str, i32, &str, i32)>) -> String {
        let mut text = format!(
            "Tick Device: mode:     1\n\
                 Broadcast device\n\
                 Clock Event Device: {broadcast}\n\
                 tick_broadcast_mask: 00\n\
                 tick_broadcast_oneshot_mask: 00\n\n"
        );
        text.push_str(&cpu(0, "lapic", 3, "hrtimer_interrupt", 1));
        if let Some((device, state, handler, mode)) = second {
            text.push_str(&cpu(1, device, state, handler, mode));
        }
        text
    }
    let selected = CpuMask::EMPTY.with(0).with(1);
    assert_eq!(
        timer_list_verdict(
            list("<NULL>", Some(("lapic", 4, "hrtimer_interrupt", 1))).as_bytes(),
            selected
        ),
        Ok(())
    );
    for (name, text) in [
        (
            "deadline",
            list(
                "<NULL>",
                Some(("lapic-deadline", 3, "hrtimer_interrupt", 1)),
            ),
        ),
        (
            "periodic tick",
            list("<NULL>", Some(("lapic", 3, "hrtimer_interrupt", 0))),
        ),
        (
            "periodic state",
            list("<NULL>", Some(("lapic", 2, "hrtimer_interrupt", 1))),
        ),
        (
            "handler",
            list("<NULL>", Some(("lapic", 3, "tick_handle_periodic", 1))),
        ),
        ("missing cpu", list("<NULL>", None)),
        (
            "broadcast",
            list("pit", Some(("lapic", 4, "hrtimer_interrupt", 1))),
        ),
    ] {
        assert!(
            timer_list_verdict(text.as_bytes(), selected).is_err(),
            "{name}"
        );
    }
}

#[test]
fn cpuinfo_rates_round_down_to_khz() {
    assert_eq!(parse_khz("2194.8439"), Some(2_194_843));
    assert_eq!(parse_khz("2194.8"), Some(2_194_800));
    assert_eq!(parse_khz("2194"), Some(2_194_000));
    assert_eq!(parse_khz("2194.8x"), None);
    let (info, verdict) = check_cpuinfo(
        b"processor\t: 1\ncpu MHz\t\t: 2194.843\n",
        CpuMask::EMPTY.with(0),
        true,
    );
    assert_eq!((info.khz[0], info.khz[1]), (None, Some(2_194_843)));
    assert_eq!(verdict, Err("cpu 0 is not in /proc/cpuinfo".to_owned()));

    const FLAGS: &str = "tsc msr pae constant_tsc rdtscp nonstop_tsc tsc_known_freq \
                             tsc_reliable hypervisor arat x2apic";
    let fixture = |flags: &str, cpus: usize| {
        (0..cpus)
            .map(|cpu| {
                format!(
                    "processor\t: {cpu}\n\
                         vendor_id\t: GenuineIntel\n\
                         cpu family\t: 6\n\
                         model\t\t: 85\n\
                         cpu MHz\t\t: 2194.843\n\
                         flags\t\t: fpu vme {flags}\n\
                         bugs\t\t: spectre_v1\n\n"
                )
            })
            .collect::<String>()
    };
    let selected = CpuMask::EMPTY.with(0).with(1);
    let (info, verdict) = check_cpuinfo(fixture(FLAGS, 2).as_bytes(), selected, true);
    assert_eq!(verdict, Ok(()));
    assert_eq!(
        (info.khz[0], info.khz[1]),
        (Some(2_194_843), Some(2_194_843))
    );
    for (name, flags) in [
        ("tsc_reliable", FLAGS.replace("tsc_reliable ", "")),
        ("deadline", format!("{FLAGS} tsc_deadline_timer")),
        ("adjust", format!("{FLAGS} tsc_adjust")),
    ] {
        assert!(
            check_cpuinfo(fixture(&flags, 2).as_bytes(), selected, true)
                .1
                .is_err(),
            "{name}"
        );
    }
    assert!(
        check_cpuinfo(fixture(FLAGS, 1).as_bytes(), selected, true)
            .1
            .is_err()
    );
}

#[test]
fn restore_record_has_a_generation_and_optional_new_cpus() {
    let cpus = CpuMask::parse(b"4-6,9").unwrap();
    assert_eq!(
        parse_restore_record(b"generation=3\nnew_cpus=4-6,9\n"),
        Some((3, cpus))
    );
    assert_eq!(
        parse_restore_record(b"generation=18446744073709551615\nnew_cpus=none\n"),
        Some((u64::MAX, CpuMask::EMPTY))
    );
    assert_eq!(
        parse_restore_record(b"generation=3\n"),
        Some((3, CpuMask::EMPTY))
    );
    for record in [
        &b""[..],
        &b"new_cpus=4\ngeneration=3\n"[..],
        b"generation=3 new_cpus=1",
        b"generation=3\nnew_cpus=7-4\n",
        b"generation=3\nnew_cpus= 1-2\n",
        b"generation=3\nnew_cpus=64\n",
        b"generation=3\nnew_cpus=\ntrailing=value\n",
    ] {
        assert_eq!(parse_restore_record(record), None, "{record:?}");
    }
}

#[test]
fn the_command_line_is_checked_as_the_kernel_reads_it() {
    // The kernel applies these parameters however the command line separates, quotes, or
    // spells them: `à` is C3 A0 in UTF-8, and the kernel takes the byte 0xa0, as it does the
    // vertical tab, for whitespace (microsoft/nvx#723).
    let cases: [(&[u8], &[u8]); 21] = [
        (b"quiet \xc3\xa0tsc=unstable", b"tsc=unstable"),
        (b"quiet\xc3\xa0tsc=unstable", b"tsc=unstable"),
        (b"quiet\x0btsc_early_khz=2100000", b"tsc_early_khz=2100000"),
        (b"quiet\xa0notsc", b"notsc"),
        (b"\"tsc=unstable\"", b"tsc=unstable"),
        (b"tsc=\"unstable\"", b"tsc=unstable"),
        (b"a \"clocksource=hpet\" b", b"clocksource=hpet"),
        (b"tsc-early-khz=2100000", b"tsc-early-khz=2100000"),
        (b"lapic-timer-hz=200000000", b"lapic-timer-hz=200000000"),
        (b"nolapic-timer", b"nolapic-timer"),
        (b"nolapic=1", b"nolapic=1"),
        (b"tsc_early_khz", b"tsc_early_khz"),
        // `__setup("notsc", ...)` takes every parameter that starts with it, and
        // `hpet_setup` every option of its list that starts with `force`.
        (b"notsc=1", b"notsc=1"),
        (b"notscx", b"notscx"),
        (b"hpet=verbose,force", b"hpet=verbose,force"),
        (b"hpet=forced", b"hpet=forced"),
        (b"clocksource=tsc clocksource=hpet", b"clocksource=hpet"),
        (b"clocksource=", b"clocksource="),
        (b"nolapic_timer\n", b"nolapic_timer"),
        (b"hpet=force\0", b"hpet=force"),
        // The newline that procfs writes after the command line is not in a quote that the
        // command line leaves open.
        (b"quiet tsc=\"unstable\n", b"tsc=unstable"),
    ];
    for (command_line, quoted) in cases {
        assert_eq!(
            check_cmdline(command_line).unwrap_err(),
            [b"command line contains ".as_slice(), quoted].concat(),
            "{}",
            command_line.escape_ascii()
        );
    }
    // The kernel leaves what follows a `--` to init, reads text in quotes as a value of
    // another parameter, and applies these parameters to no clock.
    for command_line in [
        &b"earlycon=xe9 console=hvc0 quiet clocksource=tsc tsc=reliable\n"[..],
        b"quiet -- tsc=unstable notsc",
        b"quiet\xa0--\xa0tsc=unstable",
        b"a=\"tsc=unstable notsc\" b",
        b"xnotsc tscx=unstable tsc=unstable2 hpet=disable,verbose clocksource nolapicx",
        b"quiet\xe2\x80\x83tsc=unstable",
        b"quiet\0 tsc=unstable",
        b"quiet clocksource=\"tsc\n",
        b"",
    ] {
        assert_eq!(
            check_cmdline(command_line),
            Ok(()),
            "{}",
            command_line.escape_ascii()
        );
    }
    let long = [b"notsc".as_slice(), &[b'x'; QUOTED_TOKEN_MAX]].concat();
    assert_eq!(
        check_cmdline(&long).unwrap_err(),
        [
            b"command line contains ".as_slice(),
            &long[..QUOTED_TOKEN_MAX]
        ]
        .concat()
    );
}

/// Returns whether the kernel applies, in the parameter `name` with `value`, a clock
/// parameter that the time ABI forbids, as `init/main.c` matches parameters with their
/// handlers: an `early_param` by `parameq` of its name, which NVX applies to
/// `lapic_timer_hz` too, and a `__setup` string by `parameqn` of the parameter's text over the
/// string's length, which passes the rest of the text to the handler.
fn applies_forbidden_clock(name: &[u8], value: Option<&[u8]>) -> bool {
    let line = model::line(name, value);
    let early = |string: &str| model::parameq(name, string.as_bytes());
    let setup = |string: &str| {
        model::parameqn(&line, string.as_bytes(), string.len()).then(|| &line[string.len()..])
    };
    [
        "tsc_early_khz",
        "lapic_timer_hz",
        "nolapic",
        "nolapic_timer",
    ]
    .into_iter()
    .any(early)
        || setup("notsc").is_some()
        || setup("tsc=") == Some(b"unstable".as_slice())
        || setup("hpet=").is_some_and(|argument| {
            argument
                .split(|&byte| byte == b',')
                .any(|option| option.starts_with(b"force"))
        })
        || setup("clocksource=").is_some_and(|argument| argument != b"tsc")
}

/// Returns a strategy for command lines of the clock parameters, others, their values,
/// quotes, the `--` delimiter, and the kernel's whitespace.
fn clock_command_line() -> impl Strategy<Value = Vec<u8>> {
    /// The pieces of the command lines.
    const PIECES: [&[u8]; 29] = [
        b"tsc",
        b"notsc",
        b"nolapic",
        b"_timer",
        b"-timer",
        b"tsc_early_khz",
        b"tsc-early",
        b"_khz",
        b"lapic_timer_hz",
        b"hpet",
        b"clocksource",
        b"x",
        b"=",
        b"unstable",
        b"reliable",
        b"force",
        b",",
        b"verbose",
        b"1",
        b"\"",
        b"--",
        b"-",
        b"_",
        b" ",
        b"\t",
        b"\n",
        b"\x0b",
        b"\xa0",
        b"\xc3\xa0",
    ];
    prop::collection::vec(prop::sample::select(PIECES.to_vec()), 0..16)
        .prop_map(|pieces| pieces.concat())
}

proptest! {
    /// The check of what `/proc/cmdline` holds, a command line and the newline that procfs
    /// writes after it, fails exactly when the kernel applies, in the command line, a clock
    /// parameter that the ABI forbids.
    #[test]
    fn the_check_fails_when_the_kernel_applies_a_forbidden_clock(
        command_line in clock_command_line(),
    ) {
        let applied = model::read(&command_line)
            .parameters
            .iter()
            .any(|(name, value)| applies_forbidden_clock(name, value.as_deref()));
        prop_assert_eq!(
            check_cmdline(&[command_line.as_slice(), b"\n"].concat()).is_err(),
            applied,
            "{}",
            command_line.escape_ascii()
        );
    }
}
