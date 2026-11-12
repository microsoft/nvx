use super::super::support::TscRateSource;
use super::*;

/// A TSC rate of 1 GHz, at which a cycle is a nanosecond.
const GIGAHERTZ: TscRate = TscRate {
    hz: 1e9,
    #[cfg(target_arch = "x86_64")]
    source: TscRateSource::Option,
    #[cfg(target_arch = "aarch64")]
    source: TscRateSource::CntfrqEl0,
};

#[test]
#[cfg(target_arch = "aarch64")]
fn arm_warp_uses_the_actual_counter_rate_and_restores_affinity() {
    let before = affinity_cpus().unwrap();
    let rate = architectural_rate(&before[..1]).unwrap();
    assert_eq!(
        rate.hz,
        sys::aarch64::counter_frequency_hz().unwrap() as f64
    );
    assert_eq!(rate.source, TscRateSource::CntfrqEl0);
    assert_eq!(affinity_cpus().unwrap(), before);
    assert!(matches!(architectural_rate(&[]), Err(Errno::EINVAL)));
    assert_eq!(affinity_cpus().unwrap(), before);
}

/// Returns a ping-pong leader's result whose round trips bound the offset to `lo..=hi`
/// cycles, and whose quickest round trip took `min_rtt` cycles at twice the offset
/// `min_offset_x2`.
fn leader(lo: i64, hi: i64, min_rtt: u64, min_offset_x2: i64) -> LeaderResult {
    LeaderResult {
        rounds: 100,
        min_rtt,
        min_offset_x2,
        lo,
        hi,
        stalled: false,
    }
}

/// Returns a pair's result with `offset`, or `None` for a stalled ping-pong, and `warps`
/// backward steps of at most `max_warp_cycles`.
fn pair(offset: Option<OffsetEstimate>, warps: u64, max_warp_cycles: u64) -> PairResult {
    PairResult {
        cpu_a: 0,
        cpu_b: 1,
        warp_iterations: 1000,
        warps,
        max_warp_cycles,
        offset,
    }
}

/// Returns the summary of one measured pair, whose offset is bound to `lo..=hi` cycles and
/// whose largest backward step is `max_warp_cycles`.
fn measured(lo: i64, hi: i64, max_warp_cycles: u64) -> Summary {
    let mut summary = Summary::default();
    summary.add(&pair(
        Some(OffsetEstimate::new(&leader(lo, hi, 50, 0), 0)),
        0,
        max_warp_cycles,
    ));
    summary
}

#[test]
fn cacheline_wrappers_are_really_aligned() {
    assert_eq!(std::mem::align_of::<Aligned<AtomicU64>>(), 64);
    assert!(std::mem::size_of::<Aligned<AtomicU64>>() >= 64);
}

#[test]
fn offsets_come_from_the_bounds_or_else_from_the_quickest_round_trip() {
    let consistent = OffsetEstimate::new(&leader(-10, 30, 50, 4), 6);
    assert!(consistent.consistent);
    assert_eq!(consistent.offset_cycles, 10.0);
    assert_eq!(consistent.uncertainty_cycles, 20.0);
    assert_eq!(consistent.median_offset_cycles, 3.0);
    assert_eq!(consistent.skew_bound(), 30.0);
    let inconsistent = OffsetEstimate::new(&leader(50, 20, 40, 60), 0);
    assert!(!inconsistent.consistent);
    assert_eq!(inconsistent.offset_cycles, 30.0);
    assert_eq!(inconsistent.uncertainty_cycles, 20.0);
    assert_eq!(inconsistent.skew_bound(), 50.0);
}

#[test]
fn summaries_count_stalled_and_inconsistent_pairs() {
    let mut summary = Summary::default();
    summary.add(&pair(
        Some(OffsetEstimate::new(&leader(-10, 30, 50, 4), 6)),
        0,
        0,
    ));
    summary.add(&pair(
        Some(OffsetEstimate::new(&leader(50, 20, 40, 60), 0)),
        1,
        3,
    ));
    summary.add(&pair(None, 2, 7));
    assert_eq!(summary.pairs, 3);
    assert_eq!(summary.inconsistent, 1);
    assert_eq!(summary.stalled, 1);
    // A stalled pair's spinlock test still counts.
    assert_eq!((summary.total_warps, summary.max_backward), (3, 7));
    assert_eq!(summary.max_abs_offset, 30.0);
    assert_eq!(summary.max_uncertainty, 20.0);
    assert_eq!(summary.max_skew_bound, 50.0);
    // A stalled pair fails the verdict, inconclusively, whatever the bound.
    assert_eq!(
        summary.verdict(GIGAHERTZ, u64::MAX),
        Verdict {
            pass: false,
            conclusive: false
        }
    );
}

#[test]
fn verdicts_need_every_backward_step_and_offset_within_the_bound() {
    let verdict = |pass, conclusive| Verdict { pass, conclusive };
    // The offset is 10 cycles, known to +/-20, and the largest backward step 5 cycles.
    let within = measured(-10, 30, 5);
    assert_eq!(within.verdict(GIGAHERTZ, 100), verdict(true, true));
    assert_eq!(within.verdict(GIGAHERTZ, 10), verdict(true, false));
    assert_eq!(within.verdict(GIGAHERTZ, 9), verdict(false, false));
    assert_eq!(
        measured(-10, 30, 101).verdict(GIGAHERTZ, 100),
        verdict(false, true)
    );
    assert_eq!(
        measured(150, 250, 0).verdict(GIGAHERTZ, 100),
        verdict(false, true)
    );
    assert_eq!(
        Summary::default().verdict(GIGAHERTZ, 0),
        verdict(true, true)
    );
}
