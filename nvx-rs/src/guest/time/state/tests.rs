use super::*;

const ROUND_TRIP_STATE: &str = concat!(
    "version=1\n",
    "generation=2\n",
    "boot_status=ok\n",
    "capture_status=ok\n",
    "restore_status=pending\n",
    "boot_cpus=8\n",
    "capture_cpus=8\n",
    "restore_cpus=0\n",
    "boot_elapsed_us=2400\n",
    "capture_elapsed_us=310\n",
    "restore_elapsed_us=1830\n",
    "boot_cpu_us=1900\n",
    "capture_cpu_us=290\n",
    "restore_cpu_us=640\n",
    "capture_generation=1\n",
    "restore_generation=2\n",
    "tsc_hz=2194843000\n",
    "lapic_hz=200000000\n",
    "discontinuities=3\n",
    "last_discontinuity=step\n",
    "last_step_ns=-200000000\n",
    "last_step_realtime_ns=1790841600000000000\n",
    "last_downtime_ns=31000000000\n",
    "last_downtime_source=utc\n",
    "synchronized=1\n",
    "offset_ns=-42\n",
    "uncertainty_ns=1800\n",
    "frequency_ppb=-1200\n",
    "frequency_limited=0\n",
    "host_status=synchronized\n",
    "host_correction_ppb=-1185\n",
    "host_correction_bound_ppb=2\n",
    "host_max_error_us=12500\n",
    "host_disruption=1\n",
    "samples=17\n",
    "rejected_samples=1\n",
    "last_sample_error=none\n",
    "violations=0\n"
);

const STATUS_STATE: &str = concat!(
    "version=1\n",
    "generation=2\n",
    "boot_status=ok\n",
    "capture_status=ok\n",
    "restore_status=ok\n",
    "boot_cpus=8\n",
    "capture_cpus=8\n",
    "restore_cpus=4\n",
    "boot_elapsed_us=2400\n",
    "capture_elapsed_us=310\n",
    "restore_elapsed_us=1830\n",
    "boot_cpu_us=1900\n",
    "capture_cpu_us=290\n",
    "restore_cpu_us=640\n",
    "capture_generation=1\n",
    "restore_generation=2\n",
    "tsc_hz=2194843000\n",
    "lapic_hz=200000000\n",
    "discontinuities=2\n",
    "synchronized=1\n",
    "offset_ns=-42\n",
    "uncertainty_ns=1800\n",
    "rejected_samples=2\n",
    "last_sample_error=G_SAMPLE_UNCERTAIN\n"
);

fn status(text: &str, report_only: bool) -> String {
    let state = TimeState::parse(text.as_bytes()).unwrap();
    let mut output = state.status_markers(report_only);
    output.push_str(&format!("ok={}\n", u8::from(state.checks_ok())));
    output
}

#[test]
fn fresh_state_has_only_a_pending_boot_check() {
    let state = TimeState::parse(b"version=1\n").unwrap();
    assert!(
        state
            .to_string()
            .starts_with("version=1\ngeneration=0\nboot_status=pending\nboot_cpus=0\n")
    );
    assert!(state.to_string().starts_with(
        "version=1\ngeneration=0\nboot_status=pending\nboot_cpus=0\n\
                 boot_elapsed_us=0\nboot_cpu_us=0\ntsc_hz=0\nlapic_hz=0\n"
    ));
    assert_eq!(state.checks[1].status, None);
    assert!(state.pending(true) && state.pending(false) && !state.checks_ok());
}

#[test]
fn malformed_known_values_fail_but_unknown_keys_do_not() {
    for text in [
        &b"unknown=x\nsamples=bad\n"[..],
        b"boot_status=done\n",
        b"last_discontinuity=jump\n",
        b"generation=-1\n",
        b"boot_failures=-1\n",
        b"samples=\xff\n",
        b"host_status=synced\n",
        b"host_max_error_us=-1\n",
        b"host_correction_bound_ppb=none\n",
        b"last_sample_error=G_SAMPLE_LATE\n",
    ] {
        assert_eq!(TimeState::parse(text), None, "{}", text.escape_ascii());
    }
    assert!(TimeState::parse(b"unknown=bad\n").is_some());
}

#[test]
fn host_fields_round_trip_with_unknown_bounds() {
    let state = TimeState::parse(
        b"host_status=free-running\nhost_correction_ppb=200000\n\
              host_correction_bound_ppb=unknown\nhost_max_error_us=unknown\n\
              frequency_limited=1\nlast_sample_error=G_SAMPLE_TSC\n",
    )
    .unwrap();
    assert_eq!(state.host_status, HostClockStatus::FreeRunning);
    assert_eq!(state.host_correction_ppb, 200_000);
    assert_eq!(
        (state.host_correction_bound_ppb, state.host_max_error_us),
        (None, None)
    );
    assert!(state.frequency_limited);
    assert_eq!(state.last_sample_error, SampleRejection::Tsc);
    let text = state.to_string();
    for line in [
        "\nfrequency_limited=1\nhost_status=free-running\nhost_correction_ppb=200000\n\
             host_correction_bound_ppb=unknown\nhost_max_error_us=unknown\nhost_disruption=0\n",
        "\nlast_sample_error=G_SAMPLE_TSC\n",
    ] {
        assert!(text.contains(line), "{text}");
    }
    assert_eq!(TimeState::parse(text.as_bytes()).unwrap(), state);
}

#[test]
fn an_accepted_sample_records_the_host_discipline() {
    let mut state = TimeState {
        samples: 2,
        last_sample_error: SampleRejection::Uncertain,
        ..TimeState::default()
    };
    state.record_sample(&BestSample {
        theta: -42,
        epsilon: 1_800,
        host: decoders::HostClock {
            status: HostClockStatus::Synchronized,
            disruption: 3,
            correction: 39_321_600,
            correction_bound: Some(66),
            max_error_us: Some(12_500),
        },
        frequency: 32_768_000,
        limited: true,
    });
    assert_eq!(
        (
            state.samples,
            state.offset_ns,
            state.uncertainty_ns,
            state.frequency_ppb,
            state.frequency_limited
        ),
        (3, -42, 1_800, 500_000, true)
    );
    assert_eq!(
        (
            state.host_status,
            state.host_correction_ppb,
            state.host_correction_bound_ppb,
            state.host_max_error_us,
            state.host_disruption
        ),
        (
            HostClockStatus::Synchronized,
            600_000,
            Some(2),
            Some(12_500),
            3
        )
    );
    assert_eq!(state.last_sample_error, SampleRejection::None);
}

#[test]
fn host_statuses_decode_from_their_sample_bytes() {
    assert_eq!(
        (0..=4).map(HostClockStatus::from_byte).collect::<Vec<_>>(),
        [
            Some(HostClockStatus::Unknown),
            Some(HostClockStatus::Synchronized),
            Some(HostClockStatus::FreeRunning),
            Some(HostClockStatus::Unreliable),
            None
        ]
    );
    assert_eq!(HostClockStatus::default(), HostClockStatus::Unknown);
}

#[test]
fn records_round_trip_with_their_tokens() {
    let text = "version=1\ngeneration=2\nboot_status=ok\nrestore_status=fail\nboot_cpus=8\n\
                    restore_cpus=4\nboot_elapsed_us=1\nrestore_elapsed_us=2\nboot_cpu_us=3\n\
                    restore_cpu_us=4\nrestore_generation=2\nrestore_failures=3\ntsc_hz=0\n\
                    lapic_hz=0\ndiscontinuities=1\nlast_discontinuity=restore\nlast_step_ns=-5\n\
                    last_step_realtime_ns=6\nlast_downtime_ns=7\nlast_downtime_source=monotonic\n\
                    synchronized=1\noffset_ns=-8\nuncertainty_ns=9\nfrequency_ppb=-10\n\
                    frequency_limited=0\nhost_status=unknown\nhost_correction_ppb=-11\n\
                    host_correction_bound_ppb=unknown\nhost_max_error_us=14\nhost_disruption=15\n\
                    samples=11\nrejected_samples=12\nlast_sample_error=G_SAMPLE_UNCERTAIN\n\
                    violations=13\n";
    let state = TimeState::parse(text.as_bytes()).unwrap();
    assert_eq!(state.to_string(), text);
    assert!(!state.pending(false) && !state.checks_ok());
    assert_eq!(
        state.status_markers(true).lines().next(),
        Some(
            "NVX-TIME-REPORT: v=1 phase=boot status=ok cpus=8 tsc_hz=0 lapic_hz=0 \
                 generation=0 elapsed_us=1 cpu_us=3 failures=0"
        )
    );
}

#[test]
fn enforcing_and_report_only_states_round_trip_exactly() {
    let state = TimeState::parse(ROUND_TRIP_STATE.as_bytes()).unwrap();
    assert_eq!(state.to_string(), ROUND_TRIP_STATE);
    assert!(state.pending(false) && !state.checks_ok());

    let report = ROUND_TRIP_STATE
        .replace("restore_status=pending", "restore_status=fail")
        .replace(
            "restore_generation=2\n",
            "restore_generation=2\nboot_failures=0\ncapture_failures=0\n\
                 restore_failures=3\n",
        );
    assert_eq!(
        TimeState::parse(report.as_bytes()).unwrap().to_string(),
        report
    );
}

#[test]
fn status_prints_every_recorded_check_and_the_runtime_line() {
    const RATES: &str = "tsc_hz=2194843000 lapic_hz=200000000";
    assert_eq!(
        status(STATUS_STATE, false),
        format!(
            "NVX-TIME-ABI: v=1 phase=boot status=ok cpus=8 {RATES} \
                 generation=0 elapsed_us=2400 cpu_us=1900\n\
                 NVX-TIME-ABI: v=1 phase=capture status=ok cpus=8 {RATES} \
                 generation=1 elapsed_us=310 cpu_us=290\n\
                 NVX-TIME-ABI: v=1 phase=restore status=ok cpus=4 {RATES} \
                 generation=2 elapsed_us=1830 cpu_us=640\n\
                 NVX-TIME-ABI: v=1 phase=runtime status=synchronized generation=2 \
                 discontinuities=2 offset_ns=-42 uncertainty_ns=1800 rejected_samples=2 \
                 last_sample_error=G_SAMPLE_UNCERTAIN\n\
                 ok=1\n"
        )
    );

    let pending = status(
        &STATUS_STATE.replace("restore_status=ok", "restore_status=pending"),
        false,
    );
    assert!(pending.contains(&format!(
        "NVX-TIME-ABI: v=1 phase=restore status=pending cpus=4 {RATES} generation=2\n"
    )));
    assert!(pending.ends_with("ok=0\n"));

    let fresh = concat!(
        "NVX-TIME-ABI: v=1 phase=boot status=pending cpus=0 tsc_hz=0 ",
        "lapic_hz=0 generation=0\n",
        "NVX-TIME-ABI: v=1 phase=runtime status=unsynchronized generation=0 ",
        "discontinuities=0 offset_ns=0 uncertainty_ns=0 rejected_samples=0 ",
        "last_sample_error=none\n",
        "ok=0\n"
    );
    for text in [
        "version=1\ngeneration=0\nboot_status=pending\n",
        "version=1\n",
    ] {
        assert_eq!(status(text, false), fresh, "{text:?}");
    }

    let report_state = STATUS_STATE
        .replace("restore_status=ok", "restore_status=fail")
        .replace(
            "restore_generation=2\n",
            "restore_generation=2\nboot_failures=0\nrestore_failures=3\n",
        );
    assert_eq!(
        status(&report_state, true),
        format!(
            "NVX-TIME-REPORT: v=1 phase=boot status=ok cpus=8 {RATES} \
                 generation=0 elapsed_us=2400 cpu_us=1900 failures=0\n\
                 NVX-TIME-REPORT: v=1 phase=capture status=ok cpus=8 {RATES} \
                 generation=1 elapsed_us=310 cpu_us=290 failures=0\n\
                 NVX-TIME-REPORT: v=1 phase=restore status=fail cpus=4 {RATES} \
                 generation=2 elapsed_us=1830 cpu_us=640 failures=3\n\
                 NVX-TIME-REPORT: v=1 phase=runtime status=synchronized generation=2 \
                 discontinuities=2 offset_ns=-42 uncertainty_ns=1800 rejected_samples=2 \
                 last_sample_error=G_SAMPLE_UNCERTAIN\n\
                 ok=0\n"
        )
    );
}
