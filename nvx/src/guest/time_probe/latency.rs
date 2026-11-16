//! The single-CPU instruction and clock latency probe.

use std::collections::TryReserveError;
use std::hint::black_box;
use std::io::Write;

use super::format::{f0, f1};
use super::support::{
    Arguments, TscRate, check_cpus_allowed, current_clocksource, cycles_to_ns, detect_tsc_rate,
    parse_rate,
};
use crate::guest::parse::parse_u64;
use crate::guest::status::CommandError;
use crate::guest::sys::{self, x86};

/// Timed batches that the probe measures without `--iterations`.
const DEFAULT_ITERATIONS: u64 = 2000;
/// Operations per timed batch without `--batch`.
const DEFAULT_BATCH: u64 = 50;
/// The batch counts that `--iterations` accepts.
const ITERATIONS: std::ops::RangeInclusive<u64> = 1..=10_000_000;
/// The operation counts that `--batch` accepts.
const BATCH: std::ops::RangeInclusive<u64> = 1..=1_000_000;

#[derive(Clone, Copy, PartialEq, Eq)]
/// Operation measured by one latency row.
enum Method {
    /// Unfenced `RDTSC`.
    Rdtsc,
    /// Serializing `RDTSCP`.
    Rdtscp,
    /// `LFENCE` followed by `RDTSC`.
    LfenceRdtsc,
    /// vDSO-aware monotonic clock read.
    ClockMonotonic,
    /// vDSO-aware realtime clock read.
    ClockRealtime,
    /// Direct monotonic `clock_gettime` syscall.
    SyscallMonotonic,
    /// Direct `getpid` syscall.
    SyscallGetpid,
    /// Scheduler yield syscall.
    SchedYield,
}

impl Method {
    /// Every method, in the stable measurement and output order.
    const ALL: [Self; 8] = [
        Self::Rdtsc,
        Self::Rdtscp,
        Self::LfenceRdtsc,
        Self::ClockMonotonic,
        Self::ClockRealtime,
        Self::SyscallMonotonic,
        Self::SyscallGetpid,
        Self::SchedYield,
    ];

    /// Returns the method's stable output name.
    const fn name(self) -> &'static str {
        match self {
            Self::Rdtsc => "rdtsc",
            Self::Rdtscp => "rdtscp",
            Self::LfenceRdtsc => "lfence_rdtsc",
            Self::ClockMonotonic => "clock_gettime_monotonic",
            Self::ClockRealtime => "clock_gettime_realtime",
            Self::SyscallMonotonic => "syscall_clock_gettime_monotonic",
            Self::SyscallGetpid => "syscall_getpid",
            Self::SchedYield => "sched_yield",
        }
    }
}

#[derive(Clone, Copy)]
/// Summary statistics for one measured operation.
struct ResultRow {
    /// Nearest-rank median latency in nanoseconds.
    p50: f64,
    /// Nearest-rank 99th-percentile latency in nanoseconds.
    p99: f64,
    /// Minimum latency in nanoseconds.
    min: f64,
    /// Arithmetic mean latency in nanoseconds.
    mean: f64,
    /// Number of timed batches.
    samples: usize,
}

/// Executes `operation` `batch` times in one loop of its own, and returns the sum of its results,
/// which the caller hands to `black_box` so that the optimizer keeps every operation.
fn repeat(batch: u32, mut operation: impl FnMut() -> u64) -> u64 {
    (0..batch).fold(0, |sum, _| sum.wrapping_add(operation()))
}

/// Executes `batch` copies of one operation and returns the sum of their results.
///
/// It selects the operation once, as the C probe's `switch` did, so that each operation runs in
/// a loop specialized for it and the timed region dispatches nothing.
fn run_batch(method: Method, batch: u32) -> u64 {
    /// Reads `clock` through the vDSO and returns its nanoseconds within the second.
    fn nanoseconds(clock: sys::Clock) -> u64 {
        sys::clock_time(clock).map_or(0, |time| time.tv_nsec() as u64)
    }
    match method {
        Method::Rdtsc => repeat(batch, x86::rdtsc),
        Method::Rdtscp => repeat(batch, x86::rdtscp),
        Method::LfenceRdtsc => repeat(batch, x86::rdtsc_lfence),
        Method::ClockMonotonic => repeat(batch, || nanoseconds(sys::Clock::Monotonic)),
        Method::ClockRealtime => repeat(batch, || nanoseconds(sys::Clock::Realtime)),
        Method::SyscallMonotonic => repeat(batch, sys::syscall_monotonic_nanoseconds),
        Method::SyscallGetpid => repeat(batch, sys::syscall_process_id),
        Method::SchedYield => repeat(batch, || {
            std::thread::yield_now();
            0
        }),
    }
}

/// Returns the zero-based position, in `count` sorted samples, of their nearest-rank
/// `percentile`: the smallest sample that at least that fraction of the samples does not exceed.
fn rank_index(count: usize, percentile: f64) -> usize {
    ((percentile * count as f64).ceil() as usize).clamp(1, count) - 1
}

/// Selects a nearest-rank percentile from sorted, nonempty samples.
fn nearest_rank(sorted: &[f64], percentile: f64) -> f64 {
    sorted[rank_index(sorted.len(), percentile)]
}

/// Measures the median fenced-TSC bracket overhead over `iterations` brackets.
fn bracket_overhead(iterations: usize) -> Result<f64, TryReserveError> {
    let mut samples = Vec::new();
    samples.try_reserve_exact(iterations)?;
    samples.extend((0..iterations).map(|_| {
        let t0 = x86::rdtsc_fenced();
        let t1 = x86::rdtsc_fenced();
        t1.wrapping_sub(t0) as f64
    }));
    let median = rank_index(samples.len(), 0.50);
    Ok(*samples.select_nth_unstable_by(median, f64::total_cmp).1)
}

/// Measures one method and computes its summary row.
fn measure(
    method: Method,
    iterations: usize,
    batch: u32,
    overhead: f64,
    rate: TscRate,
) -> Result<ResultRow, TryReserveError> {
    let mut per_op = Vec::new();
    per_op.try_reserve_exact(iterations)?;
    black_box(run_batch(method, batch));
    for _ in 0..iterations {
        let t0 = x86::rdtsc_fenced();
        let sum = run_batch(method, batch);
        let t1 = x86::rdtsc_fenced();
        black_box(sum);
        let cycles = (t1.wrapping_sub(t0) as f64 - overhead).max(0.0);
        per_op.push(cycles_to_ns(cycles, rate) / f64::from(batch));
    }
    // The mean adds the samples in measurement order, as the C probe did, before they are sorted.
    let mean = per_op.iter().sum::<f64>() / iterations as f64;
    per_op.sort_unstable_by(f64::total_cmp);
    Ok(ResultRow {
        p50: nearest_rank(&per_op, 0.50),
        p99: nearest_rank(&per_op, 0.99),
        min: per_op[0],
        mean,
        samples: iterations,
    })
}

/// Parses options and prints every latency measurement.
pub(super) fn run(
    arguments: &[&[u8]],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    let mut iterations = DEFAULT_ITERATIONS;
    let mut batch = DEFAULT_BATCH;
    let mut cpu = None;
    let mut override_hz = None;
    let mut arguments = Arguments::new(b"latency", arguments);
    while let Some(argument) = arguments.next() {
        if let Some(value) = arguments.value(argument, b"--iterations") {
            iterations = parse_u64(value)
                .filter(|value| ITERATIONS.contains(value))
                .ok_or_else(|| arguments.invalid(err))?;
        } else if let Some(value) = arguments.value(argument, b"--batch") {
            batch = parse_u64(value)
                .filter(|value| BATCH.contains(value))
                .ok_or_else(|| arguments.invalid(err))?;
        } else if let Some(value) = arguments.value(argument, b"--cpu") {
            let parsed = parse_u64(value)
                .and_then(|cpu| usize::try_from(cpu).ok())
                .filter(|cpu| *cpu < super::MAX_CPUS);
            cpu = Some(parsed.ok_or_else(|| arguments.invalid(err))?);
        } else if let Some(value) = arguments.value(argument, b"--tsc-hz") {
            override_hz = Some(parse_rate(value).ok_or_else(|| arguments.invalid(err))?);
        } else {
            return Err(arguments.invalid(err));
        }
    }

    let cpu = cpu.map_or_else(sys::current_cpu, Ok);
    if let Ok(cpu) = cpu {
        check_cpus_allowed(b"latency", &[cpu], err)?;
    }
    // The C probe printed -1 for a CPU that `sched_getcpu` could not report.
    let cpu_label = cpu.map_or(-1, |cpu| cpu as i64);
    if let Err(error) = cpu.and_then(sys::pin_current_thread) {
        super::report(
            err,
            &[
                format!("nvx-time-probe latency: cannot pin to cpu {cpu_label}: ").as_bytes(),
                error.desc().as_bytes(),
                b"\n",
            ],
        );
        return Err(CommandError::Setup);
    }
    let rate = detect_tsc_rate(override_hz);
    if rate.hz <= 0.0 {
        super::report(
            err,
            &[b"nvx-time-probe latency: unknown TSC rate; pass --tsc-hz\n"],
        );
        return Err(CommandError::Setup);
    }
    let clocksource = current_clocksource();
    let mut out_of_memory = |_: TryReserveError| {
        super::report(err, &[b"nvx-time-probe latency: out of memory\n"]);
        CommandError::Setup
    };
    let overhead = bracket_overhead(iterations as usize).map_err(&mut out_of_memory)?;
    let vdso = sys::kernel::has_vdso();
    let _ = writeln!(
        out,
        "nvx-time-probe {} latency: cpu={cpu_label} iterations={iterations} batch={batch} \
         tsc_hz={} tsc_hz_source={} clocksource={} vdso={} bracket_overhead_ns={}",
        super::VERSION,
        f0(rate.hz),
        rate.source,
        String::from_utf8_lossy(&clocksource),
        u8::from(vdso),
        f1(cycles_to_ns(overhead, rate))
    );
    let _ = writeln!(
        out,
        "{:<32} {:>10} {:>10} {:>10} {:>10}",
        "method", "p50_ns", "p99_ns", "min_ns", "mean_ns"
    );
    let mut results = [None; Method::ALL.len()];
    let rdtscp = x86::has_rdtscp();
    for (method, result) in Method::ALL.into_iter().zip(&mut results) {
        let name = method.name();
        if method == Method::Rdtscp && !rdtscp {
            let _ = writeln!(
                out,
                "{name:<32} unsupported (CPUID.80000001H:EDX[27] clear)"
            );
            continue;
        }
        let row = measure(method, iterations as usize, batch as u32, overhead, rate)
            .map_err(&mut out_of_memory)?;
        *result = Some(row);
        let _ = writeln!(
            out,
            "{name:<32} {:>10} {:>10} {:>10} {:>10}",
            f1(row.p50),
            f1(row.p99),
            f1(row.min),
            f1(row.mean)
        );
        let _ = out.flush();
    }
    for (method, result) in Method::ALL.into_iter().zip(results) {
        let name = method.name();
        let Some(row) = result else {
            let _ = writeln!(out, "NVX-TIME-PROBE latency method={name} unsupported=1");
            continue;
        };
        let _ = writeln!(
            out,
            "NVX-TIME-PROBE latency method={name} p50_ns={} p99_ns={} min_ns={} \
             mean_ns={} samples={} batch={batch} cpu={cpu_label} clocksource={}",
            f1(row.p50),
            f1(row.p99),
            f1(row.min),
            f1(row.mean),
            row.samples,
            String::from_utf8_lossy(&clocksource)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;
