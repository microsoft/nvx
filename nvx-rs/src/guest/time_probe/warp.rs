//! Pairwise cross-CPU TSC warp and offset checks.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread;

use super::format::{f0, f1};
use super::support::{
    Arguments, NANOSECONDS_PER_SECOND, TscRate, affinity_cpus, check_cpus_allowed, cycles_to_ns,
    format_cpu_list, parse_cpu_list, round_ll,
};
#[cfg(target_arch = "x86_64")]
use super::support::{detect_tsc_rate, parse_rate};
use crate::guest::parse::parse_u64;
use crate::guest::status::CommandError;
#[cfg(target_arch = "x86_64")]
use crate::guest::sys::x86::pause;
use crate::guest::sys::{self, Errno, TimeValLike};
#[cfg(target_arch = "aarch64")]
use std::hint::spin_loop as pause;

/// Counter units in the ISA-specific diagnostic, without changing x86 output.
#[cfg(target_arch = "x86_64")]
const COUNTER_HZ_FIELD: &str = "tsc_hz";
#[cfg(target_arch = "aarch64")]
const COUNTER_HZ_FIELD: &str = "counter_hz";

/// Reads the counter used by the unchanged pairwise algorithms.
fn read_counter() -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        sys::x86::rdtsc()
    }
    #[cfg(target_arch = "aarch64")]
    {
        sys::aarch64::virtual_counter()
    }
}

/// Orders the counter read after preceding accesses.
fn read_counter_ordered() -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        sys::x86::rdtsc_lfence()
    }
    #[cfg(target_arch = "aarch64")]
    {
        sys::aarch64::virtual_counter()
    }
}

/// Brackets the counter read with the ISA's existing barriers.
fn read_counter_fenced() -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        sys::x86::rdtsc_fenced()
    }
    #[cfg(target_arch = "aarch64")]
    {
        sys::aarch64::virtual_counter()
    }
}

/// Requires the actual nominal counter frequency to agree on every selected CPU.
#[cfg(target_arch = "aarch64")]
fn architectural_rate(cpus: &[usize]) -> Result<TscRate, Errno> {
    let _affinity = sys::AffinityGuard::save()?;
    let mut frequency = None;
    for &cpu in cpus {
        sys::pin_current_thread(cpu)?;
        let current = sys::aarch64::counter_frequency_hz()?;
        if frequency.is_some_and(|previous| previous != current) {
            return Err(Errno::EPROTO);
        }
        frequency = Some(current);
    }
    Ok(TscRate {
        hz: frequency.ok_or(Errno::EINVAL)? as f64,
        source: super::support::TscRateSource::CntfrqEl0,
    })
}

/// Milliseconds that each test runs per CPU pair without `--duration-ms`.
const DEFAULT_DURATION_MS: u64 = 100;
/// The durations in milliseconds that `--duration-ms` accepts.
const DURATIONS_MS: std::ops::RangeInclusive<u64> = 1..=600_000;
/// The verdict's bound on skew in nanoseconds without `--bound-ns`.
const DEFAULT_BOUND_NS: u64 = 1000;
/// Milliseconds in a second, which scale the duration to cycles.
const MILLISECONDS_PER_SECOND: f64 = 1000.0;
/// Spinlock iterations between the leader's checks of its deadline.
const WARP_CHECK_MASK: u64 = 7;
/// Spinlock iterations between the leader's checks of its stall deadline.
const WARP_STALL_CHECK_MASK: u64 = 4095;
/// Ping-pong rounds between the leader's checks of its deadline.
const PING_CHECK_MASK: u64 = 63;

/// Ping-pong sequence value that asks the follower to stop.
const PP_STOP: u64 = u64::MAX;
/// Maximum stored offset samples per CPU pair.
const PP_MAX_SAMPLES: usize = 1 << 18;
/// Extra wall-clock allowance before a worker is considered stalled.
const STALL_SECONDS: f64 = 2.0;
/// Spin-iteration mask controlling periodic stall checks.
const SPIN_CHECK_MASK: u64 = 0x0f_ffff;

#[repr(align(64))]
#[derive(Default)]
/// Cache-line-aligned owner preventing false sharing of hot atomics.
struct Aligned<T>(T);

/// Shared state for the spinlock lower-bound warp test, cleared for each CPU pair.
#[derive(Default)]
struct WarpState {
    /// Test-and-set spinlock on its own cache line.
    lock: Aligned<AtomicBool>,
    /// TSC read by the previous lock owner.
    last_tsc: AtomicU64,
    /// Largest observed backward delta.
    max_warp: AtomicU64,
    /// Number of backward TSC observations.
    warps: AtomicU64,
    /// Two-thread startup barrier on its own cache line: the workers that reached it.
    ready: Aligned<AtomicU32>,
    /// Whether a worker could not start.
    abort: AtomicBool,
    /// Whether the leader ended the test.
    stop: AtomicBool,
}

/// Shared state for the ping-pong offset estimate, cleared for each CPU pair.
#[derive(Default)]
struct PingState {
    /// Release/acquire request and response sequence.
    sequence: Aligned<AtomicU64>,
    /// Follower timestamp paired with the current sequence.
    t2: AtomicU64,
    /// Two-thread startup barrier on its own cache line: the workers that reached it.
    ready: Aligned<AtomicU32>,
    /// Whether a worker could not start.
    abort: AtomicBool,
}

/// The ping-pong estimate of one CPU pair's TSC offset, in cycles.
struct OffsetEstimate {
    /// Completed ping-pong rounds.
    rounds: u64,
    /// Smallest round trip.
    min_rtt_cycles: u64,
    /// Greatest lower offset bound.
    lo_cycles: i64,
    /// Least upper offset bound.
    hi_cycles: i64,
    /// Nearest-median sampled offset.
    median_offset_cycles: f64,
    /// Selected offset estimate: the middle of the bounds, or the smallest round trip's offset
    /// when the bounds do not intersect.
    offset_cycles: f64,
    /// Selected half-width uncertainty.
    uncertainty_cycles: f64,
    /// Whether all ping-pong interval bounds intersect.
    consistent: bool,
}

impl OffsetEstimate {
    /// Selects the estimate from the leader's bounds and the median of its sampled offsets, each
    /// of them twice an offset.
    fn new(leader: &LeaderResult, median_x2: i64) -> Self {
        let consistent = leader.lo <= leader.hi;
        let (offset_cycles, uncertainty_cycles) = if consistent {
            (
                (leader.lo as f64 + leader.hi as f64) / 2.0,
                (leader.hi as f64 - leader.lo as f64) / 2.0,
            )
        } else {
            (
                leader.min_offset_x2 as f64 / 2.0,
                leader.min_rtt as f64 / 2.0,
            )
        };
        Self {
            rounds: leader.rounds,
            min_rtt_cycles: leader.min_rtt,
            lo_cycles: leader.lo,
            hi_cycles: leader.hi,
            median_offset_cycles: median_x2 as f64 / 2.0,
            offset_cycles,
            uncertainty_cycles,
            consistent,
        }
    }

    /// Returns the largest skew that the estimate admits, in cycles.
    fn skew_bound(&self) -> f64 {
        if self.consistent {
            (self.lo_cycles as f64)
                .abs()
                .max((self.hi_cycles as f64).abs())
        } else {
            self.offset_cycles.abs() + self.uncertainty_cycles
        }
    }
}

/// Complete warp and offset result for one unordered CPU pair.
struct PairResult {
    /// Leader CPU.
    cpu_a: usize,
    /// Follower CPU.
    cpu_b: usize,
    /// Combined spinlock-test iterations.
    warp_iterations: u64,
    /// Number of backward TSC observations.
    warps: u64,
    /// Largest backward delta in cycles.
    max_warp_cycles: u64,
    /// The ping-pong offset estimate, or `None` when a worker exceeded its stall bound.
    offset: Option<OffsetEstimate>,
}

/// The verdict's inputs, accumulated over every CPU pair.
#[derive(Default)]
struct Summary {
    /// Pairs measured.
    pairs: usize,
    /// Pairs whose ping-pong bounds did not intersect.
    inconsistent: usize,
    /// Pairs whose ping-pong test stalled.
    stalled: usize,
    /// Backward TSC observations of every pair.
    total_warps: u64,
    /// Largest backward TSC delta, in cycles.
    max_backward: u64,
    /// Largest absolute offset estimate, in cycles.
    max_abs_offset: f64,
    /// Largest offset uncertainty, in cycles.
    max_uncertainty: f64,
    /// Largest skew bound, in cycles.
    max_skew_bound: f64,
}

impl Summary {
    /// Accounts for one pair's result.
    fn add(&mut self, result: &PairResult) {
        self.pairs += 1;
        self.total_warps += result.warps;
        self.max_backward = self.max_backward.max(result.max_warp_cycles);
        let Some(offset) = &result.offset else {
            self.stalled += 1;
            return;
        };
        if !offset.consistent {
            self.inconsistent += 1;
        }
        self.max_abs_offset = self.max_abs_offset.max(offset.offset_cycles.abs());
        self.max_uncertainty = self.max_uncertainty.max(offset.uncertainty_cycles);
        self.max_skew_bound = self.max_skew_bound.max(offset.skew_bound());
    }

    /// Judges the pairs, whose TSCs run at `rate`, against the skew bound `bound_ns`.
    fn verdict(&self, rate: TscRate, bound_ns: u64) -> Verdict {
        let bound = bound_ns as f64;
        Verdict {
            pass: self.stalled == 0
                && cycles_to_ns(self.max_backward as f64, rate) <= bound
                && cycles_to_ns(self.max_abs_offset, rate) <= bound,
            conclusive: self.pairs == 0
                || (self.stalled == 0 && cycles_to_ns(self.max_uncertainty, rate) < bound),
        }
    }
}

/// The verdict on every pair against the skew bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Verdict {
    /// Whether no pair stalled, and no backward step or offset exceeds the bound.
    pass: bool,
    /// Whether the verdict holds: there are no pairs, or none stalled and every offset is known
    /// more precisely than the bound.
    conclusive: bool,
}

/// Synchronizes two pinned workers, and returns whether both started: a worker whose pinning
/// `failed` aborts both.
fn barrier(ready: &AtomicU32, abort: &AtomicBool, failed: bool) -> bool {
    if failed {
        abort.store(true, Ordering::SeqCst);
    }
    ready.fetch_add(1, Ordering::SeqCst);
    while ready.load(Ordering::SeqCst) < 2 {
        pause();
    }
    !abort.load(Ordering::SeqCst)
}

/// Reads coarse monotonic time for infrequent stall detection, or zero when the clock cannot be
/// read.
fn coarse_ns() -> u64 {
    sys::clock_time(sys::Clock::MonotonicCoarse).map_or(0, |time| time.num_nanoseconds() as u64)
}

/// Returns the `errno` of a thread that could not start, or `EAGAIN`, as `pthread_create`
/// returns for a lack of resources.
fn spawn_errno(error: &io::Error) -> Errno {
    error.raw_os_error().map_or(Errno::EAGAIN, Errno::from_raw)
}

/// Runs one side of the spinlock-based TSC monotonicity test.
fn warp_worker(
    state: &WarpState,
    cpu: usize,
    leader: bool,
    budget_cycles: u64,
    rate: TscRate,
) -> Result<u64, Errno> {
    let pinned = sys::pin_current_thread(cpu);
    if !barrier(&state.ready.0, &state.abort, pinned.is_err()) {
        return pinned.map(|()| 0);
    }
    let end = read_counter_ordered().wrapping_add(budget_cycles);
    let deadline = coarse_ns().wrapping_add(
        (cycles_to_ns(budget_cycles as f64, rate) + STALL_SECONDS * NANOSECONDS_PER_SECOND) as u64,
    );
    let mut iteration = 0_u64;
    loop {
        while state.lock.0.swap(true, Ordering::Acquire) {
            while state.lock.0.load(Ordering::Relaxed) {
                pause();
            }
        }
        let previous = state.last_tsc.load(Ordering::Relaxed);
        let now = read_counter_ordered();
        state.last_tsc.store(now, Ordering::Relaxed);
        if previous > now {
            state.max_warp.fetch_max(previous - now, Ordering::Relaxed);
            state.warps.fetch_add(1, Ordering::Relaxed);
        }
        state.lock.0.store(false, Ordering::Release);
        if iteration & WARP_CHECK_MASK == 0 {
            if leader
                && (read_counter() > end
                    || (iteration & WARP_STALL_CHECK_MASK == 0 && coarse_ns() > deadline))
            {
                state.stop.store(true, Ordering::Relaxed);
            }
            if state.stop.load(Ordering::Relaxed) {
                break;
            }
        }
        iteration += 1;
    }
    Ok(iteration + 1)
}

/// Leader-only aggregate from the ping-pong test.
struct LeaderResult {
    /// Completed request/response rounds.
    rounds: u64,
    /// Smallest round trip in cycles.
    min_rtt: u64,
    /// Twice the offset associated with the smallest round trip.
    min_offset_x2: i64,
    /// Greatest lower offset bound.
    lo: i64,
    /// Least upper offset bound.
    hi: i64,
    /// Whether the follower stalled.
    stalled: bool,
}

/// Runs the ping-pong leader, records offset intervals, and adds twice each round's offset to
/// `offsets`, up to `PP_MAX_SAMPLES` of them.
fn ping_leader(
    state: &PingState,
    cpu: usize,
    budget_cycles: u64,
    stall_cycles: u64,
    offsets: &mut Vec<i64>,
) -> Result<LeaderResult, Errno> {
    let pinned = sys::pin_current_thread(cpu);
    if !barrier(&state.ready.0, &state.abort, pinned.is_err()) {
        return Err(pinned.err().unwrap_or(Errno::EINVAL));
    }
    let end = read_counter_ordered().wrapping_add(budget_cycles);
    let mut result = LeaderResult {
        rounds: 0,
        min_rtt: u64::MAX,
        min_offset_x2: 0,
        lo: i64::MIN,
        hi: i64::MAX,
        stalled: false,
    };
    let mut round = 1_u64;
    loop {
        let mut spins = 0_u64;
        let t1 = read_counter_fenced();
        state.sequence.0.store(2 * round - 1, Ordering::Release);
        while state.sequence.0.load(Ordering::Acquire) != 2 * round {
            spins += 1;
            if spins & SPIN_CHECK_MASK == 0 && read_counter().wrapping_sub(t1) > stall_cycles {
                result.stalled = true;
                state.sequence.0.store(PP_STOP, Ordering::Release);
                return Ok(result);
            }
            pause();
        }
        let t3 = read_counter_ordered();
        let t2 = state.t2.load(Ordering::Relaxed);
        let rtt = t3.wrapping_sub(t1);
        let lo = t2.wrapping_sub(t3) as i64;
        let hi = t2.wrapping_sub(t1) as i64;
        if rtt < result.min_rtt {
            result.min_rtt = rtt;
            result.min_offset_x2 = lo.wrapping_add(hi);
        }
        result.lo = result.lo.max(lo);
        result.hi = result.hi.min(hi);
        if offsets.len() < PP_MAX_SAMPLES {
            offsets.push(lo.wrapping_add(hi));
        }
        result.rounds = round;
        if round & PING_CHECK_MASK == 0 && read_counter() > end {
            state.sequence.0.store(PP_STOP, Ordering::Release);
            return Ok(result);
        }
        round += 1;
    }
}

/// Runs the ping-pong follower on its pinned CPU.
fn ping_follower(state: &PingState, cpu: usize, stall_cycles: u64) -> Result<(), Errno> {
    let pinned = sys::pin_current_thread(cpu);
    if !barrier(&state.ready.0, &state.abort, pinned.is_err()) {
        return pinned;
    }
    let mut round = 1_u64;
    loop {
        let expected = 2 * round - 1;
        let mut spins = 0_u64;
        let mut wait_start = 0_u64;
        loop {
            let value = state.sequence.0.load(Ordering::Acquire);
            if value == expected {
                break;
            }
            if value == PP_STOP {
                return Ok(());
            }
            spins += 1;
            if spins & SPIN_CHECK_MASK == 0 {
                let now = read_counter();
                if wait_start == 0 {
                    wait_start = now;
                } else if now.wrapping_sub(wait_start) > stall_cycles {
                    return Ok(());
                }
            }
            pause();
        }
        state.t2.store(read_counter_fenced(), Ordering::Relaxed);
        state.sequence.0.store(2 * round, Ordering::Release);
        round += 1;
    }
}

/// Runs both pairwise tests with scoped, joined worker threads, sampling the ping-pong offsets
/// into `offsets`, whose capacity holds `PP_MAX_SAMPLES` of them.
fn run_pair(
    cpu_a: usize,
    cpu_b: usize,
    budget_cycles: u64,
    stall_cycles: u64,
    offsets: &mut Vec<i64>,
    rate: TscRate,
) -> Result<PairResult, Errno> {
    let warp = WarpState::default();
    let (first, second) = thread::scope(|scope| {
        let first = thread::Builder::new()
            .spawn_scoped(scope, || {
                warp_worker(&warp, cpu_a, true, budget_cycles, rate)
            })
            .map_err(|error| spawn_errno(&error))?;
        let second = match thread::Builder::new().spawn_scoped(scope, || {
            warp_worker(&warp, cpu_b, false, budget_cycles, rate)
        }) {
            Ok(thread) => thread,
            Err(error) => {
                warp.abort.store(true, Ordering::SeqCst);
                warp.ready.0.fetch_add(1, Ordering::SeqCst);
                let _ = first.join();
                return Err(spawn_errno(&error));
            }
        };
        let first = first.join().map_err(|_| Errno::EIO)??;
        let second = second.join().map_err(|_| Errno::EIO)??;
        Ok((first, second))
    })?;
    let ping = PingState::default();
    offsets.clear();
    let leader = thread::scope(|scope| {
        let leader = thread::Builder::new()
            .spawn_scoped(scope, || {
                ping_leader(&ping, cpu_a, budget_cycles, stall_cycles, offsets)
            })
            .map_err(|error| spawn_errno(&error))?;
        let follower = match thread::Builder::new()
            .spawn_scoped(scope, || ping_follower(&ping, cpu_b, stall_cycles))
        {
            Ok(thread) => thread,
            Err(error) => {
                ping.abort.store(true, Ordering::SeqCst);
                ping.ready.0.fetch_add(1, Ordering::SeqCst);
                let _ = leader.join();
                return Err(spawn_errno(&error));
            }
        };
        let leader = leader.join().map_err(|_| Errno::EIO)??;
        follower.join().map_err(|_| Errno::EIO)??;
        Ok(leader)
    })?;
    let offset = (!leader.stalled && !offsets.is_empty()).then(|| {
        let middle = (offsets.len() - 1) / 2;
        OffsetEstimate::new(&leader, *offsets.select_nth_unstable(middle).1)
    });
    Ok(PairResult {
        cpu_a,
        cpu_b,
        warp_iterations: first + second,
        warps: warp.warps.load(Ordering::Relaxed),
        max_warp_cycles: warp.max_warp.load(Ordering::Relaxed),
        offset,
    })
}

/// Parses options and runs every unordered CPU pair.
pub(super) fn run(
    arguments: &[&[u8]],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    let mut duration_ms = DEFAULT_DURATION_MS;
    let mut bound_ns = DEFAULT_BOUND_NS;
    #[cfg(target_arch = "x86_64")]
    let mut override_hz = None;
    let mut cpus = None;
    let mut arguments = Arguments::new(b"warp", arguments);
    while let Some(argument) = arguments.next() {
        if let Some(value) = arguments.value(argument, b"--duration-ms") {
            duration_ms = parse_u64(value)
                .filter(|value| DURATIONS_MS.contains(value))
                .ok_or_else(|| arguments.invalid(err))?;
        } else if let Some(value) = arguments.value(argument, b"--cpus") {
            let parsed = parse_cpu_list(value).filter(|cpus| !cpus.is_empty());
            cpus = Some(parsed.ok_or_else(|| arguments.invalid(err))?);
        } else if let Some(value) = arguments.value(argument, b"--bound-ns") {
            bound_ns = parse_u64(value).ok_or_else(|| arguments.invalid(err))?;
        } else if let Some(value) = arguments.value(argument, b"--tsc-hz") {
            #[cfg(target_arch = "x86_64")]
            {
                override_hz = Some(parse_rate(value).ok_or_else(|| arguments.invalid(err))?);
            }
            #[cfg(target_arch = "aarch64")]
            {
                let _ = value;
                return Err(arguments.invalid(err));
            }
        } else {
            return Err(arguments.invalid(err));
        }
    }
    // Linux never reports an empty affinity mask, which would be `EINVAL` to set.
    let cpus = cpus
        .map_or_else(affinity_cpus, Ok)
        .and_then(|cpus| {
            if cpus.is_empty() {
                Err(Errno::EINVAL)
            } else {
                Ok(cpus)
            }
        })
        .map_err(|error| {
            super::report(
                err,
                &[
                    b"nvx-time-probe warp: sched_getaffinity: ",
                    error.desc().as_bytes(),
                    b"\n",
                ],
            );
            CommandError::Setup
        })?;
    check_cpus_allowed(b"warp", &cpus, err)?;
    #[cfg(target_arch = "x86_64")]
    let rate = detect_tsc_rate(override_hz);
    #[cfg(target_arch = "aarch64")]
    let rate = architectural_rate(&cpus).map_err(|error| {
        super::report(
            err,
            &[
                b"nvx-time-probe warp: invalid per-CPU CNTFRQ_EL0: ",
                error.desc().as_bytes(),
                b"\n",
            ],
        );
        CommandError::Setup
    })?;
    #[cfg(target_arch = "x86_64")]
    if rate.hz <= 0.0 {
        super::report(
            err,
            &[b"nvx-time-probe warp: unknown TSC rate; pass --tsc-hz\n"],
        );
        return Err(CommandError::Setup);
    }
    let budget_cycles = (duration_ms as f64 * rate.hz / MILLISECONDS_PER_SECOND) as u64;
    let stall_cycles = (STALL_SECONDS * rate.hz) as u64 + budget_cycles;
    let mut offsets = Vec::new();
    if offsets.try_reserve_exact(PP_MAX_SAMPLES).is_err() {
        super::report(err, &[b"nvx-time-probe warp: out of memory\n"]);
        return Err(CommandError::Setup);
    }
    let cpu_text = format_cpu_list(&cpus);
    let expected_pairs = cpus.len() * cpus.len().saturating_sub(1) / 2;
    let _ = writeln!(
        out,
        "nvx-time-probe {} warp: cpus={cpu_text} pairs={expected_pairs} duration_ms={duration_ms} \
         (per test per pair) {COUNTER_HZ_FIELD}={} {COUNTER_HZ_FIELD}_source={} bound_ns={bound_ns}",
        super::VERSION,
        f0(rate.hz),
        rate.source
    );
    let _ = out.flush();

    let mut summary = Summary::default();
    for (first, &cpu_a) in cpus.iter().enumerate() {
        for &cpu_b in &cpus[first + 1..] {
            let result = match run_pair(
                cpu_a,
                cpu_b,
                budget_cycles,
                stall_cycles,
                &mut offsets,
                rate,
            ) {
                Ok(result) => result,
                Err(error) => {
                    super::report(
                        err,
                        &[
                            format!("nvx-time-probe warp: pair {cpu_a}-{cpu_b}: ").as_bytes(),
                            error.desc().as_bytes(),
                            b"\n",
                        ],
                    );
                    return Err(CommandError::Setup);
                }
            };
            summary.add(&result);
            let Some(offset) = &result.offset else {
                let _ = writeln!(
                    out,
                    "pair {}-{}: warp_iterations={} warps={} max_backward_cycles={} \
                     ping_pong=stalled",
                    result.cpu_a,
                    result.cpu_b,
                    result.warp_iterations,
                    result.warps,
                    result.max_warp_cycles
                );
                continue;
            };
            let _ = writeln!(
                out,
                "pair {}-{}: warp_iterations={} warps={} max_backward_cycles={} \
                 max_backward_ns={} rounds={} min_rtt_ns={} offset_cycles={} offset_ns={} \
                 uncertainty_ns={} bounds_ns=[{},{}] median_offset_ns={} consistent={}",
                result.cpu_a,
                result.cpu_b,
                result.warp_iterations,
                result.warps,
                result.max_warp_cycles,
                round_ll(cycles_to_ns(result.max_warp_cycles as f64, rate)),
                offset.rounds,
                round_ll(cycles_to_ns(offset.min_rtt_cycles as f64, rate)),
                f1(offset.offset_cycles),
                round_ll(cycles_to_ns(offset.offset_cycles, rate)),
                round_ll(cycles_to_ns(offset.uncertainty_cycles, rate)),
                round_ll(cycles_to_ns(offset.lo_cycles as f64, rate)),
                round_ll(cycles_to_ns(offset.hi_cycles as f64, rate)),
                round_ll(cycles_to_ns(offset.median_offset_cycles, rate)),
                u8::from(offset.consistent)
            );
            let _ = out.flush();
        }
    }
    let Verdict { pass, conclusive } = summary.verdict(rate, bound_ns);
    let Summary {
        pairs,
        inconsistent,
        stalled,
        total_warps,
        max_backward,
        max_abs_offset,
        max_uncertainty,
        max_skew_bound,
    } = summary;
    if pairs == 0 {
        let _ = writeln!(out, "single CPU: no pairs to compare");
    }
    if !conclusive {
        let read_name = if cfg!(target_arch = "x86_64") {
            "RDTSC"
        } else {
            "CNTVCT_EL0"
        };
        let _ = writeln!(
            out,
            "warning: offset uncertainty (+/-{} ns) is not below the bound ({bound_ns} ns); \
             the verdict is not conclusive (trapped {read_name} or a descheduled vCPU?)",
            round_ll(cycles_to_ns(max_uncertainty, rate))
        );
    }
    let _ = writeln!(
        out,
        "NVX-TIME-PROBE warp max_backward_cycles={max_backward} max_backward_ns={} \
         max_abs_offset_ns={} pairs={pairs} verdict={} bound_ns={bound_ns}",
        round_ll(cycles_to_ns(max_backward as f64, rate)),
        round_ll(cycles_to_ns(max_abs_offset, rate)),
        if pass { "PASS" } else { "FAIL" }
    );
    let _ = writeln!(
        out,
        "NVX-TIME-PROBE warp-detail max_abs_offset_cycles={} max_uncertainty_ns={} \
         max_skew_bound_ns={} total_warps={total_warps} inconsistent_pairs={inconsistent} \
         stalled_pairs={stalled} cpus={cpu_text} duration_ms={duration_ms} {COUNTER_HZ_FIELD}={} \
         {COUNTER_HZ_FIELD}_source={} conclusive={}",
        f1(max_abs_offset),
        round_ll(cycles_to_ns(max_uncertainty, rate)),
        round_ll(cycles_to_ns(max_skew_bound, rate)),
        f0(rate.hz),
        rate.source,
        u8::from(conclusive)
    );
    if pass {
        Ok(())
    } else {
        Err(CommandError::Operation)
    }
}

#[cfg(test)]
mod tests;
