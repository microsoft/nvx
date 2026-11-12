//! Option parsing, affinity, kernel-log access, and TSC-rate discovery.

use std::fmt;
use std::io::Write;
#[cfg(target_arch = "x86_64")]
use std::{sync::OnceLock, time::Duration};

use crate::guest::kernel_text;
#[cfg(target_arch = "x86_64")]
use crate::guest::kernel_text::{KmsgRecord, SyslogLine};
#[cfg(target_arch = "x86_64")]
use crate::guest::parse::{parse_decimal, parse_f64};
use crate::guest::status::CommandError;
use crate::guest::sys::{self, Errno};
#[cfg(target_arch = "x86_64")]
use crate::guest::sys::{Mode, OpenFlags, TimeValLike, kernel};

use super::MAX_CPUS;

/// The largest structured `/dev/kmsg` record, the kernel's `CONSOLE_EXT_LOG_MAX`.
#[cfg(target_arch = "x86_64")]
const KMSG_RECORD_MAX: usize = 8192;
/// Size of the buffer for the kernel's log when the kernel does not report the log's size.
#[cfg(target_arch = "x86_64")]
const FALLBACK_LOG_SIZE: usize = 1 << 20;
/// Nanoseconds in a second, which scale the selected counter's cycles.
pub(super) const NANOSECONDS_PER_SECOND: f64 = 1e9;
/// kHz in a MHz, and Hz in a kHz.
#[cfg(target_arch = "x86_64")]
const THOUSAND: f64 = 1000.0;
/// The interval over which the probe measures the TSC rate when nothing reports it.
#[cfg(target_arch = "x86_64")]
const MEASURE_INTERVAL: Duration = Duration::from_millis(200);

/// Where the probe found the selected ISA's counter rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TscRateSource {
    /// The `--tsc-hz` option.
    #[cfg(target_arch = "x86_64")]
    Option,
    /// The kernel's `tsc_freq_khz` sysfs attribute.
    #[cfg(target_arch = "x86_64")]
    SysfsTscFreqKhz,
    /// The kernel log's refined TSC calibration.
    #[cfg(target_arch = "x86_64")]
    KmsgRefined,
    /// The kernel log's detected TSC rate.
    #[cfg(target_arch = "x86_64")]
    KmsgDetectedTsc,
    /// The kernel log's detected processor rate.
    #[cfg(target_arch = "x86_64")]
    KmsgDetectedProcessor,
    /// The first `cpu MHz` of `/proc/cpuinfo`.
    #[cfg(target_arch = "x86_64")]
    CpuinfoCpuMhz,
    /// A measurement against `CLOCK_MONOTONIC_RAW`.
    #[cfg(target_arch = "x86_64")]
    MeasuredMonotonicRaw,
    /// The actual architectural nominal frequency, without a guessed or measured fallback.
    #[cfg(target_arch = "aarch64")]
    CntfrqEl0,
}

impl fmt::Display for TscRateSource {
    /// Writes the source's stable output token.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            #[cfg(target_arch = "x86_64")]
            Self::Option => "option",
            #[cfg(target_arch = "x86_64")]
            Self::SysfsTscFreqKhz => "sysfs-tsc_freq_khz",
            #[cfg(target_arch = "x86_64")]
            Self::KmsgRefined => "kmsg-refined",
            #[cfg(target_arch = "x86_64")]
            Self::KmsgDetectedTsc => "kmsg-detected-tsc",
            #[cfg(target_arch = "x86_64")]
            Self::KmsgDetectedProcessor => "kmsg-detected-processor",
            #[cfg(target_arch = "x86_64")]
            Self::CpuinfoCpuMhz => "cpuinfo-cpu-mhz",
            #[cfg(target_arch = "x86_64")]
            Self::MeasuredMonotonicRaw => "measured-monotonic-raw",
            #[cfg(target_arch = "aarch64")]
            Self::CntfrqEl0 => "cntfrq_el0",
        })
    }
}

/// Counter frequency and the source that supplied it.
#[derive(Clone, Copy)]
pub(super) struct TscRate {
    /// Cycles per second, zero for a measurement that could not read its clock.
    pub(super) hz: f64,
    /// Where the rate came from.
    pub(super) source: TscRateSource,
}

/// Parses a positive, finite rate option, such as `--tsc-hz`, in decimal floating point.
#[cfg(target_arch = "x86_64")]
pub(super) fn parse_rate(text: &[u8]) -> Option<f64> {
    parse_f64(text).filter(|value| *value > 0.0)
}

/// A probe command's arguments, which it reads one option at a time.
pub(super) struct Arguments<'a> {
    /// The command, which the diagnostic of an invalid option or value names.
    command: &'static [u8],
    /// The arguments not read yet.
    rest: std::slice::Iter<'a, &'a [u8]>,
    /// The last argument read, which the diagnostic of an invalid option or value quotes.
    last: &'a [u8],
}

impl<'a> Arguments<'a> {
    /// Starts reading the `arguments` of `command`.
    pub(super) fn new(command: &'static [u8], arguments: &'a [&'a [u8]]) -> Self {
        Self {
            command,
            rest: arguments.iter(),
            last: &[],
        }
    }

    /// Returns the value of the option `name` when `argument` is that option: the text after
    /// `name=`, or for exactly `name`, the next argument, which it reads.
    pub(super) fn value(&mut self, argument: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
        match argument.strip_prefix(name)? {
            [] => self.next(),
            [b'=', value @ ..] => Some(value),
            _ => None,
        }
    }

    /// Prints the byte-exact diagnostic that quotes the last argument read as an invalid option
    /// or value, and fails with the usage status.
    ///
    /// [`value`](Self::value) reads nothing when it returns `None`, so the last argument read is
    /// the option itself when no call took a value for it.
    pub(super) fn invalid(&self, err: &mut dyn Write) -> CommandError {
        sys::report(
            err,
            &[
                b"nvx-time-probe ",
                self.command,
                b": invalid option or value: ",
                self.last,
                b"\n",
            ],
        );
        CommandError::Usage
    }
}

impl<'a> Iterator for Arguments<'a> {
    type Item = &'a [u8];

    /// Reads the next argument.
    fn next(&mut self) -> Option<Self::Item> {
        self.last = self.rest.next()?;
        Some(self.last)
    }
}

/// Returns the CPUs in the calling thread's affinity mask.
pub(super) fn affinity_cpus() -> Result<Vec<usize>, Errno> {
    sys::current_affinity().map(|set| set.cpus())
}

/// Checks that each of `cpus` is in the calling thread's affinity mask, and reports the first
/// that is not, or the mask's error, as a usage error.
pub(super) fn check_cpus_allowed(
    command: &[u8],
    cpus: &[usize],
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    let allowed = sys::current_affinity().map_err(|error| {
        sys::report(
            err,
            &[
                b"nvx-time-probe ",
                command,
                b": sched_getaffinity: ",
                error.desc().as_bytes(),
                b"\n",
            ],
        );
        CommandError::Usage
    })?;
    match cpus.iter().find(|&&cpu| !allowed.contains(cpu)) {
        None => Ok(()),
        Some(cpu) => {
            let detail = format!(": cpu {cpu} is not in the affinity mask (offline or excluded)\n");
            sys::report(err, &[b"nvx-time-probe ", command, detail.as_bytes()]);
            Err(CommandError::Usage)
        }
    }
}

/// Parses a CPU list, such as `0-3,6`, that names each CPU below `MAX_CPUS` at most once, into
/// its CPUs in the list's order.
pub(super) fn parse_cpu_list(text: &[u8]) -> Option<Vec<usize>> {
    let ranges = kernel_text::parse_cpu_list(text)?;
    if ranges.iter().any(|range| *range.end() >= MAX_CPUS) {
        return None;
    }
    let mut cpus = Vec::new();
    for cpu in ranges.into_iter().flatten() {
        if cpus.contains(&cpu) {
            return None;
        }
        cpus.push(cpu);
    }
    Some(cpus)
}

/// Formats CPU numbers in the CPU-list syntax, with each run of consecutive CPUs as a range.
pub(super) fn format_cpu_list(cpus: &[usize]) -> String {
    cpus.chunk_by(|first, second| first + 1 == *second)
        .map(|run| match (run.first(), run.last()) {
            (Some(first), Some(last)) if first != last => format!("{first}-{last}"),
            (Some(cpu), _) => cpu.to_string(),
            (None, _) => String::new(),
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Parses a decimal number in the form in which the kernel prints rates, such as `2194.843`:
/// digits with an optional fraction, after optional spaces and tabs.
#[cfg(target_arch = "x86_64")]
fn parse_kernel_decimal(text: &[u8]) -> Option<f64> {
    let number = std::str::from_utf8(text)
        .ok()?
        .trim_start_matches([' ', '\t']);
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    let digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
    if whole.is_empty() || !digits(whole) || !digits(fraction) {
        return None;
    }
    parse_f64(number.as_bytes())
}

/// Iterates the records of the nonblocking `/dev/kmsg`, skipping those that the kernel overwrote
/// before the probe read them. Fails with the error that ended the reading when it read no
/// record, such as `EAGAIN` for a log without records.
#[cfg(target_arch = "x86_64")]
fn kmsg_from_dev(mut consume: impl FnMut(KmsgRecord<'_>)) -> Result<(), Errno> {
    let log = sys::open(
        kernel::KERNEL_LOG_DEVICE,
        OpenFlags::O_RDONLY | OpenFlags::O_NONBLOCK | OpenFlags::O_CLOEXEC,
        Mode::empty(),
    )?;
    let mut count = 0;
    let mut buffer = [0_u8; KMSG_RECORD_MAX];
    let error = loop {
        let read = match log.read(&mut buffer) {
            Ok(0) => break Errno::ENODATA,
            Ok(read) => read,
            // A record that the kernel overwrote is skipped, as the C probe skipped it.
            Err(Errno::EINTR | Errno::EPIPE) => continue,
            Err(error) => break error,
        };
        if let Some(record) = KmsgRecord::parse(&buffer[..read]) {
            consume(record);
            count += 1;
        }
    };
    if count > 0 { Ok(()) } else { Err(error) }
}

/// Iterates the lines of the whole `syslog(2)` buffer, which the probe reads when `/dev/kmsg`
/// yields no records.
#[cfg(target_arch = "x86_64")]
fn kmsg_from_syslog(mut consume: impl FnMut(KmsgRecord<'_>)) -> Result<(), Errno> {
    let size = kernel::kernel_log_size()
        .ok()
        .filter(|size| *size > 0)
        .unwrap_or(FALLBACK_LOG_SIZE);
    let mut buffer = Vec::new();
    buffer.try_reserve_exact(size).map_err(|_| Errno::ENOMEM)?;
    buffer.resize(size, 0);
    let count = kernel::read_kernel_log(&mut buffer)?;
    for line in buffer[..count]
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let line = SyslogLine::parse(line);
        consume(KmsgRecord {
            timestamp_us: line.timestamp_us,
            message: line.message,
        });
    }
    Ok(())
}

/// Iterates kernel messages, preferring structured `/dev/kmsg`.
#[cfg(target_arch = "x86_64")]
pub(super) fn kmsg_foreach(mut consume: impl FnMut(KmsgRecord<'_>)) -> Result<(), Errno> {
    kmsg_from_dev(&mut consume).or_else(|_| kmsg_from_syslog(consume))
}

/// Reads `/proc/cpuinfo` once, as raw bytes, which are empty when it cannot be read.
#[cfg(target_arch = "x86_64")]
fn cpuinfo() -> &'static [u8] {
    static TEXT: OnceLock<Vec<u8>> = OnceLock::new();
    TEXT.get_or_init(|| sys::read_file(kernel::CPU_INFORMATION).unwrap_or_default())
}

/// Returns the value of the first `/proc/cpuinfo` line whose key starts with `key`.
#[cfg(target_arch = "x86_64")]
fn cpuinfo_value(key: &[u8]) -> Option<&'static [u8]> {
    cpuinfo()
        .split(|&byte| byte == b'\n')
        .filter(|line| line.starts_with(key))
        .find_map(|line| {
            let colon = line.iter().position(|&byte| byte == b':')?;
            Some(&line[colon + 1..])
        })
}

/// Lazily extracts CPU-zero feature-flag words.
#[cfg(target_arch = "x86_64")]
fn cpu_flags() -> &'static [Vec<u8>] {
    static FLAGS: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    FLAGS.get_or_init(|| {
        cpuinfo_value(b"flags").map_or_else(Vec::new, |words| {
            words
                .split(u8::is_ascii_whitespace)
                .filter(|word| !word.is_empty())
                .map(<[u8]>::to_vec)
                .collect()
        })
    })
}

/// Returns whether cpuinfo contains one exact feature flag.
#[cfg(target_arch = "x86_64")]
pub(super) fn cpuinfo_has_flag(flag: &[u8]) -> bool {
    cpu_flags().iter().any(|word| word == flag)
}

/// Returns the first positive cpuinfo MHz value.
#[cfg(target_arch = "x86_64")]
fn cpuinfo_mhz() -> Option<f64> {
    cpuinfo_value(b"cpu MHz")
        .and_then(parse_kernel_decimal)
        .filter(|&mhz| mhz > 0.0)
}

/// Returns the positive MHz value between a kernel message's `prefix` and `suffix`.
#[cfg(target_arch = "x86_64")]
fn mhz_after(text: &[u8], prefix: &[u8], suffix: &[u8]) -> Option<f64> {
    let start = kernel_text::find(text, prefix)? + prefix.len();
    let value = &text[start..];
    let end = kernel_text::find(value, suffix)?;
    parse_kernel_decimal(&value[..end]).filter(|&mhz| mhz > 0.0)
}

/// Converts MHz to whole kHz, rounded, and then to hertz, as the C probe did.
#[cfg(target_arch = "x86_64")]
fn mhz_to_hz(mhz: f64) -> f64 {
    (mhz * THOUSAND).round() * THOUSAND
}

/// Measures the TSC rate against the raw monotonic clock over `interval`, or returns zero when
/// the clock cannot be read.
#[cfg(target_arch = "x86_64")]
pub(super) fn measure_tsc_hz(interval: Duration) -> f64 {
    let first = sys::clock_time(sys::Clock::MonotonicRaw);
    let t0 = sys::x86::rdtsc_fenced();
    sys::sleep_once(interval);
    let second = sys::clock_time(sys::Clock::MonotonicRaw);
    let t1 = sys::x86::rdtsc_fenced();
    let (Ok(first), Ok(second)) = (first, second) else {
        return 0.0;
    };
    let ns = (second - first).num_nanoseconds() as f64;
    if ns > 0.0 {
        t1.wrapping_sub(t0) as f64 * NANOSECONDS_PER_SECOND / ns
    } else {
        0.0
    }
}

/// Returns the TSC rate that the kernel exposes in sysfs, when it does.
#[cfg(target_arch = "x86_64")]
fn sysfs_tsc_rate() -> Option<TscRate> {
    let khz = parse_decimal(&sys::read_line(kernel::TSC_FREQUENCY_KHZ).ok()?)?;
    (khz > 0).then_some(TscRate {
        hz: khz as f64 * THOUSAND,
        source: TscRateSource::SysfsTscFreqKhz,
    })
}

/// Returns the TSC rate that the kernel logged: its last refined calibration, or else its last
/// detected TSC rate, or else its last detected processor rate.
#[cfg(target_arch = "x86_64")]
fn kmsg_tsc_rate() -> Option<TscRate> {
    let (mut refined, mut tsc, mut processor) = (None, None, None);
    let _ = kmsg_foreach(|record| {
        let text = record.message;
        if let Some(mhz) = mhz_after(text, b"Refined TSC clocksource calibration: ", b" MHz") {
            refined = Some(mhz);
        } else if let Some(mhz) = mhz_after(text, b"tsc: Detected ", b" MHz TSC") {
            tsc = Some(mhz);
        } else if let Some(mhz) = mhz_after(text, b"tsc: Detected ", b" MHz processor") {
            processor = Some(mhz);
        }
    });
    [
        (refined, TscRateSource::KmsgRefined),
        (tsc, TscRateSource::KmsgDetectedTsc),
        (processor, TscRateSource::KmsgDetectedProcessor),
    ]
    .into_iter()
    .find_map(|(mhz, source)| {
        mhz.map(|mhz| TscRate {
            hz: mhz_to_hz(mhz),
            source,
        })
    })
}

/// Returns the rate of `/proc/cpuinfo`'s first `cpu MHz`, unless the `aperfmperf` flag shows that
/// the processor's frequency varies.
#[cfg(target_arch = "x86_64")]
fn cpuinfo_tsc_rate() -> Option<TscRate> {
    let mhz = cpuinfo_mhz()?;
    (!cpuinfo_has_flag(b"aperfmperf")).then(|| TscRate {
        hz: mhz_to_hz(mhz),
        source: TscRateSource::CpuinfoCpuMhz,
    })
}

/// Detects the TSC rate from the `--tsc-hz` option, then sysfs, then the kernel log, then
/// cpuinfo, and measures it as a last resort.
#[cfg(target_arch = "x86_64")]
pub(super) fn detect_tsc_rate(override_hz: Option<f64>) -> TscRate {
    override_hz
        .map(|hz| TscRate {
            hz,
            source: TscRateSource::Option,
        })
        .or_else(sysfs_tsc_rate)
        .or_else(kmsg_tsc_rate)
        .or_else(cpuinfo_tsc_rate)
        .unwrap_or_else(|| TscRate {
            hz: measure_tsc_hz(MEASURE_INTERVAL),
            source: TscRateSource::MeasuredMonotonicRaw,
        })
}

/// Reads the active clocksource, or `unknown`.
#[cfg(target_arch = "x86_64")]
pub(super) fn current_clocksource() -> Vec<u8> {
    sys::read_line(kernel::CURRENT_CLOCKSOURCE).unwrap_or_else(|_| b"unknown".to_vec())
}

/// Converts TSC cycles to nanoseconds using `rate`.
pub(super) fn cycles_to_ns(cycles: f64, rate: TscRate) -> f64 {
    if rate.hz > 0.0 {
        cycles * NANOSECONDS_PER_SECOND / rate.hz
    } else {
        0.0
    }
}

/// Rounds a floating-point value with C `llround` half-away-from-zero behavior.
pub(super) fn round_ll(value: f64) -> i64 {
    value.round() as i64
}

#[cfg(test)]
mod tests;
