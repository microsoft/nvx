//! Checks of the clocks against the standard library's.

use std::time::Instant;

use super::*;

#[test]
fn advances_with_the_standard_monotonic_clock() {
    let started = monotonic_ns().unwrap();
    let instant = Instant::now();
    std::thread::sleep(Duration::from_millis(2));
    let elapsed = instant.elapsed();
    let ended = monotonic_ns().unwrap();
    assert!(started > 0);
    assert!(
        ended - started >= elapsed.as_nanos() as u64,
        "{started} {ended} {elapsed:?}"
    );
}

#[test]
fn milliseconds_truncate_the_nanoseconds() {
    let before = monotonic_ns().unwrap() / NANOSECONDS_PER_MILLISECOND;
    let milliseconds = monotonic_ms().unwrap();
    let after = monotonic_ns().unwrap() / NANOSECONDS_PER_MILLISECOND;
    assert!(
        (before..=after).contains(&milliseconds),
        "{before} {milliseconds} {after}"
    );
}

#[test]
fn every_clock_reads_within_its_second() {
    for clock in [
        Clock::Realtime,
        Clock::Monotonic,
        Clock::MonotonicRaw,
        Clock::MonotonicCoarse,
        Clock::Boottime,
        Clock::ProcessCpuTime,
    ] {
        let time = clock_time(clock).unwrap();
        assert!(
            (0..NANOSECONDS_PER_SECOND as i64).contains(&time.tv_nsec()),
            "{clock:?} {time:?}"
        );
    }
}

#[test]
fn sleeps_reach_their_deadlines() {
    let start = clock_time(Clock::Monotonic).unwrap().num_nanoseconds();
    let deadline = start + 2 * NANOSECONDS_PER_MILLISECOND as i64;
    sleep_until_monotonic(deadline);
    assert!(clock_time(Clock::Monotonic).unwrap().num_nanoseconds() >= deadline);
    sleep_until_monotonic(start);
    let instant = Instant::now();
    sleep_once(Duration::from_millis(1));
    assert!(instant.elapsed() >= Duration::from_millis(1));
}

#[cfg(target_arch = "x86_64")]
#[test]
fn the_system_call_reads_nanoseconds_within_the_second() {
    assert!(syscall_monotonic_nanoseconds() < NANOSECONDS_PER_SECOND);
}
