//! Tests of eligibility: the restore window of a capture's time contract at a host's time, by the
//! clock that a restore chooses within one boot, and across a reboot.

use super::*;

/// Nanoseconds in a second.
const SECOND: u64 = 1_000_000_000;

/// Returns the time contract of a capture by the host whose identity's bytes are all `host`,
/// during its boot whose bytes are all `boot`, by Linux's boot-time clock, with no interval from
/// the cut of its VM time to its anchor.
fn contract(host: u8, boot: u8) -> TimeContract {
    TimeContract {
        tsc_frequency_hz: 2_000_000_000,
        tsc_tolerance_ppm: 100,
        apic_frequency_hz: 1_000_000_000,
        capture_tsc: 1_000,
        capture_utc_ns: 1_700_000_000 * SECOND,
        capture_monotonic_ns: 100 * SECOND,
        host_clock: HostClock::LinuxBoottime,
        host_id: [host; 16],
        boot_id: BootId::from_bytes([boot; 16]),
        lineage: 1,
        cut_to_anchor_ns: None,
    }
}

/// Returns the time of the host whose identity's bytes are all `host`, during its boot whose bytes
/// are all `boot`, by Linux's boot-time clock: `utc` seconds after the anchor of [`contract`] by
/// UTC, and `monotonic` seconds after it by the monotonic clock.
fn at(host: u8, boot: u8, utc: i64, monotonic: i64) -> HostTime {
    let capture = contract(host, boot);
    let offset = |seconds: i64| seconds * SECOND as i64;
    HostTime {
        host_id: [host; 16],
        boot_id: BootId::from_bytes([boot; 16]),
        clock: HostClock::LinuxBoottime,
        monotonic_ns: capture
            .capture_monotonic_ns
            .checked_add_signed(offset(monotonic))
            .unwrap(),
        utc_ns: capture
            .capture_utc_ns
            .checked_add_signed(offset(utc))
            .unwrap(),
    }
}

/// Returns `time` with both of its clocks `nanos` later.
fn later(time: HostTime, nanos: u64) -> HostTime {
    HostTime {
        monotonic_ns: time.monotonic_ns + nanos,
        utc_ns: time.utc_ns + nanos,
        ..time
    }
}

/// Returns a downtime of `nanos` by `source`, which advances VM time by as much.
fn downtime(nanos: u64, source: DowntimeSource) -> Downtime {
    Downtime {
        nanos,
        vm_time_nanos: nanos,
        source,
    }
}

#[test]
fn within_one_boot_the_downtime_is_the_host_monotonic_clocks() {
    let capture = contract(1, 1);
    // Steps of the host's wall clock, forward or back, change nothing within one boot.
    for utc in [5, 7, -3_600, 365 * 86_400] {
        assert_eq!(
            capture.eligibility(&at(1, 1, utc, 5)),
            Eligibility::Eligible(downtime(5 * SECOND, DowntimeSource::HostMonotonic)),
            "{utc}"
        );
    }
}

#[test]
fn across_a_reboot_on_another_host_or_by_another_clock_the_downtime_is_utcs() {
    let capture = contract(1, 1);
    let mut other_clock = at(1, 1, 7, 0);
    other_clock.clock = HostClock::WindowsInterruptTime;
    // A reboot restarts the monotonic clock, whose reading then tells nothing.
    let rebooted = HostTime {
        monotonic_ns: 1,
        ..at(1, 2, 7, 0)
    };
    for now in [at(1, 2, 7, 0), rebooted, at(2, 1, 7, 0), other_clock] {
        assert_eq!(
            capture.eligibility(&now),
            Eligibility::Eligible(downtime(7 * SECOND, DowntimeSource::Utc)),
            "{now:?}"
        );
    }
}

#[test]
fn the_window_closes_after_the_longest_downtime() {
    let capture = contract(1, 1);
    for (start, source) in [
        (at(1, 1, 0, 0), DowntimeSource::HostMonotonic),
        (at(1, 2, 0, 0), DowntimeSource::Utc),
    ] {
        for nanos in [0, 1, MAX_DOWNTIME_NS - 1, MAX_DOWNTIME_NS] {
            assert_eq!(
                capture.eligibility(&later(start, nanos)),
                Eligibility::Eligible(downtime(nanos, source)),
                "{source:?} {nanos}"
            );
        }
        for nanos in [MAX_DOWNTIME_NS + 1, 2 * MAX_DOWNTIME_NS] {
            let eligibility = capture.eligibility(&later(start, nanos));
            assert_eq!(
                eligibility,
                Eligibility::Expired(downtime(nanos, source)),
                "{source:?} {nanos}"
            );
            assert!(!eligibility.is_eligible());
        }
    }
}

#[test]
fn the_window_counts_the_interval_from_the_cut_of_the_vm_time() {
    let mut capture = contract(1, 1);
    capture.cut_to_anchor_ns = Some(200);
    let start = at(1, 1, 0, 0);
    let advanced = |nanos: u64, vm_time_nanos: u64| Downtime {
        nanos,
        vm_time_nanos,
        source: DowntimeSource::HostMonotonic,
    };
    assert_eq!(
        capture.eligibility(&start),
        Eligibility::Eligible(advanced(0, 200))
    );
    assert_eq!(
        capture.eligibility(&later(start, MAX_DOWNTIME_NS - 200)),
        Eligibility::Eligible(advanced(MAX_DOWNTIME_NS - 200, MAX_DOWNTIME_NS))
    );
    assert_eq!(
        capture.eligibility(&later(start, MAX_DOWNTIME_NS - 199)),
        Eligibility::Expired(advanced(MAX_DOWNTIME_NS - 199, MAX_DOWNTIME_NS + 1))
    );
    // An interval that is longer than the window by itself leaves no time to restore in.
    capture.cut_to_anchor_ns = Some(MAX_DOWNTIME_NS + 1);
    assert_eq!(
        capture.eligibility(&start),
        Eligibility::Expired(advanced(0, MAX_DOWNTIME_NS + 1))
    );
    capture.cut_to_anchor_ns = Some(u64::MAX);
    assert_eq!(
        capture.eligibility(&later(start, 1)),
        Eligibility::Expired(advanced(1, u64::MAX))
    );
}

#[test]
fn a_host_time_before_the_anchor_is_early() {
    let capture = contract(1, 1);
    let mut same_boot = at(1, 1, 0, 0);
    same_boot.monotonic_ns -= 1;
    let eligibility = capture.eligibility(&same_boot);
    assert_eq!(
        eligibility,
        Eligibility::Early {
            source: DowntimeSource::HostMonotonic,
            before_ns: 1,
        }
    );
    assert!(!eligibility.is_eligible());
    // Another host whose UTC is behind the capturing host's.
    assert_eq!(
        capture.eligibility(&at(2, 2, -10, 50)),
        Eligibility::Early {
            source: DowntimeSource::Utc,
            before_ns: 10 * SECOND,
        }
    );
    // The earliest reading of each clock.
    assert_eq!(
        capture.eligibility(&HostTime {
            monotonic_ns: 0,
            ..at(1, 1, 0, 0)
        }),
        Eligibility::Early {
            source: DowntimeSource::HostMonotonic,
            before_ns: capture.capture_monotonic_ns,
        }
    );
    assert_eq!(
        capture.eligibility(&HostTime {
            utc_ns: 0,
            ..at(2, 2, 0, 0)
        }),
        Eligibility::Early {
            source: DowntimeSource::Utc,
            before_ns: capture.capture_utc_ns,
        }
    );
}

#[test]
fn eligibilities_say_why() {
    let capture = contract(1, 1);
    assert_eq!(
        capture.eligibility(&at(1, 1, 0, 5)).to_string(),
        "eligible, after a downtime of 5000000000 ns by the host's monotonic clock"
    );
    assert_eq!(
        capture.eligibility(&at(2, 2, -10, 0)).to_string(),
        "not eligible yet: by UTC, the host's time is 10000000000 ns before the anchor of the \
         capture"
    );
    assert_eq!(
        capture
            .eligibility(&later(at(2, 2, 0, 0), MAX_DOWNTIME_NS + 1))
            .to_string(),
        format!(
            "no longer eligible: after a downtime of {} ns by UTC, a restore would advance the \
             clone's VM time by {} ns, more than the {MAX_DOWNTIME_NS} ns that it accepts",
            MAX_DOWNTIME_NS + 1,
            MAX_DOWNTIME_NS + 1
        )
    );
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn this_host_samples_its_time_as_a_capture_records_it() {
    let host_id = match platform::host_id() {
        Ok(host_id) => host_id,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            eprintln!("skipped: this host has no machine identity: {error}");
            return;
        }
        Err(error) => panic!("{error}"),
    };
    let first = HostTime::now().unwrap();
    assert_eq!(first.host_id, host_id);
    assert_eq!(first.boot_id, BootId::current().unwrap());
    let clock = if cfg!(windows) {
        HostClock::WindowsInterruptTime
    } else {
        HostClock::LinuxBoottime
    };
    assert_eq!(first.clock, clock);
    // After 2020-01-01.
    assert!(first.utc_ns > 1_577_836_800 * SECOND);
    let second = HostTime::now().unwrap();
    assert!(second.monotonic_ns >= first.monotonic_ns);
    // A capture at the first sample is eligible at the second, by the host's monotonic clock.
    let capture = TimeContract {
        capture_utc_ns: first.utc_ns,
        capture_monotonic_ns: first.monotonic_ns,
        host_clock: first.clock,
        host_id: first.host_id,
        boot_id: first.boot_id,
        ..contract(0, 0)
    };
    assert_eq!(
        capture.eligibility(&second),
        Eligibility::Eligible(downtime(
            second.monotonic_ns - first.monotonic_ns,
            DowntimeSource::HostMonotonic
        ))
    );
}

#[cfg(not(any(target_os = "linux", windows)))]
#[test]
fn other_hosts_sample_no_time() {
    assert_eq!(HostTime::now().unwrap_err().kind(), ErrorKind::Unavailable);
}
