//! The `nvx-time test` compatibility interface: the C program's test hooks, which exercise its
//! decoders, formats, and process structure without the hypervisor, and `port-access`, which
//! checks in a VM that the processes that the time tools start cannot reach OpenVMM's ports.
//!
//! A hook prints a decoded failure as its output and succeeds; it fails with the usage status
//! only when its own command line is malformed: a wrong number of arguments, a fixture that it
//! cannot read, a check name, code, or source that is not UTF-8, or a number that is not plain
//! decimal, with a `-` sign when it is negative. The CPUID values of `exhaustive-leaf` may also
//! be `0x`-prefixed hexadecimal, as other guest programs read their arguments. The packet and
//! sample hooks read their fixtures as port B delivers a record, zero-filled past its end.
//! `console` writes its whole line, as the program writes every event, which is shorter than
//! the 638 bytes after which the C program's buffer cut an asynchronous line.

use std::ffi::{CStr, CString, OsStr};
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::str::FromStr;
use std::sync::mpsc;

use super::cpu_mask::CpuMask;
use super::daemon::{self, Promoter};
use super::decoders::{
    self, ClockRates, GENERATION_ID_SIZE, GuestReads, PACKET_HEADER_SIZE, PacketHeader,
    RestorePacket, SAMPLE_SIZE, TimeSample,
};
use super::events::{CheckId, DETAIL_MAX, EventSource, ViolationCode, escape_text, format_event};
use super::state::TimeState;
use super::system::{self, Runtime, SampleError, clock_ns, cpu_time_ns};
use super::{DEFERRED_START_NS, IDLE_BOUND_NS, NSEC_PER_MSEC, NSEC_PER_SEC, Phase};
use crate::guest::parse::{parse_integer, parse_u64};
use crate::guest::status::CommandError;
use crate::guest::sys::x86::CpuidResult;
use crate::guest::sys::{
    self, ChildStatus, ClockAdjustment, Errno, ForkResult, Port, PortAccess, Signal,
};

/// How long after its start the `promote` hook's first promoter promotes the threads.
const PROMOTION_DELAY_NS: i64 = 50 * NSEC_PER_MSEC;
/// How long after its start the `promote` hook's second promoter would promote the threads,
/// long enough that stopping it first cancels the promotion.
const CANCELLED_PROMOTION_DELAY_NS: i64 = 10 * NSEC_PER_SEC;
/// The ports that `port-access` probes, one in each range that a port transaction grants, by
/// the names that `port-probe` takes. Reading either changes nothing in OpenVMM.
const PROBED_PORTS: [(&CStr, Port); 2] = [
    (c"portb-status", Port::PortbStatus),
    (c"snapshot", Port::Snapshot),
];

/// Runs a named test hook. The C interface exits 0 for a decoded failure and 2 only when the
/// hook's invocation itself is malformed.
pub(super) fn run(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    let Some((&name, arguments)) = arguments.split_first() else {
        return Err(CommandError::Usage);
    };
    match name {
        b"packet" => test_packet(arguments, out),
        b"sample" => test_sample(arguments, out),
        b"pair" => test_pair(arguments, out),
        b"kmsg" => test_lines(arguments, LineMode::Kmsg, out),
        b"boot-log" => test_lines(arguments, LineMode::BootLog, out),
        b"syslog" => test_lines(arguments, LineMode::Syslog, out),
        b"console" => test_console(arguments, out),
        b"exhaustive-leaf" => test_exhaustive_leaf(arguments, out),
        b"exhaustive-report" => test_exhaustive_report(arguments, out),
        b"exhaustive-summary" => test_exhaustive_summary(arguments, out),
        b"timer-list" => test_timer_list(arguments, out),
        b"correction" => test_correction(arguments, out),
        b"frequency" => test_frequency(arguments, out),
        b"event" => test_event(arguments, out),
        b"state" => test_state(arguments, out),
        b"status" => test_status(arguments, out),
        b"restore-record" => test_restore_record(arguments, out),
        b"deferred-restore-cpu" => test_deferred_restore_cpu(arguments, out),
        b"deferral" if arguments.is_empty() => {
            let _ = writeln!(
                out,
                "deferred_start_ms={} idle_bound_ms={}",
                DEFERRED_START_NS / NSEC_PER_MSEC,
                IDLE_BOUND_NS / NSEC_PER_MSEC
            );
            Ok(())
        }
        b"parallel" => test_parallel(arguments, out),
        b"promote" if arguments.is_empty() => test_promote(out),
        b"idle-priority" if arguments.is_empty() => test_idle_priority(out),
        b"cpuinfo" => test_cpuinfo(arguments, out),
        b"cmdline" => test_cmdline(arguments, out),
        b"clocksource" => test_clocksource(arguments, out),
        b"port-access" if arguments.is_empty() => test_port_access(out),
        b"port-probe" => test_port_probe(arguments),
        _ => Err(CommandError::Usage),
    }
}

/// Returns a hook's arguments when it received exactly `N` of them.
fn exactly<'a, const N: usize>(arguments: &[&'a [u8]]) -> Result<[&'a [u8]; N], CommandError> {
    arguments.try_into().map_err(|_| CommandError::Usage)
}

/// Reads a hook's text argument, which must be UTF-8.
fn utf8(argument: &[u8]) -> Result<&str, CommandError> {
    std::str::from_utf8(argument).map_err(|_| CommandError::Usage)
}

/// Reads a hook's numeric argument.
fn number<T: FromStr>(argument: &[u8]) -> Result<T, CommandError> {
    parse_integer(argument).ok_or(CommandError::Usage)
}

/// Reads a test fixture.
fn read_fixture(path: &[u8]) -> Result<Vec<u8>, CommandError> {
    fs::read(OsStr::from_bytes(path)).map_err(|_| CommandError::Usage)
}

/// Returns the `size` bytes of a binary fixture at `offset`, zero-filled past the fixture's end,
/// as port B delivers a record.
fn record(fixture: &[u8], offset: usize, size: usize) -> Vec<u8> {
    let mut record = fixture.get(offset..).unwrap_or_default().to_vec();
    record.resize(size, 0);
    record
}

/// Decodes and prints one synthetic restore packet.
fn test_packet(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    let [path, recorded, previous] = exactly(arguments)?;
    let bytes = read_fixture(path)?;
    let previous =
        decoders::parse_hex::<GENERATION_ID_SIZE>(previous).ok_or(CommandError::Usage)?;
    let recorded: u64 = number(recorded)?;
    let header = match PacketHeader::parse(&record(&bytes, 0, PACKET_HEADER_SIZE)) {
        Ok(header) => header,
        Err(error) => {
            let _ = writeln!(out, "error {} {error}", ViolationCode::RepairPacket);
            return Ok(());
        }
    };
    if u64::from(header.generation) == recorded {
        let _ = writeln!(out, "stale");
        return Ok(());
    }
    if recorded.checked_add(1) != Some(u64::from(header.generation)) {
        let _ = writeln!(
            out,
            "error {} generation {}",
            ViolationCode::RepairGeneration,
            header.generation
        );
        return Ok(());
    }
    let body = record(&bytes, PACKET_HEADER_SIZE, header.body_size());
    let packet = match RestorePacket::parse(header, &body) {
        Ok(packet) => packet,
        Err(error) => {
            let _ = writeln!(out, "error {} {error}", ViolationCode::RepairPacket);
            return Ok(());
        }
    };
    if packet.generation_id() == previous {
        let _ = writeln!(
            out,
            "error {} generation ID unchanged",
            ViolationCode::RepairGeneration
        );
        return Ok(());
    }
    let _ = write!(
        out,
        "ok flags={} online={} ranges={} generation={} rate_deviation={} downtime_ns={} \
         generation_id={}",
        header.flags,
        header.online_vp_count,
        header.range_count,
        header.generation,
        header.rate_deviation,
        header.downtime_ns,
        decoders::format_hex(packet.generation_id())
    );
    for (start, length) in packet.ranges {
        let _ = write!(out, " range={start}:{length}");
    }
    let _ = writeln!(out);
    Ok(())
}

/// Decodes and prints one synthetic time sample.
fn test_sample(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    let [path, generation] = exactly(arguments)?;
    let bytes = record(&read_fixture(path)?, 0, SAMPLE_SIZE);
    let generation: u64 = number(generation)?;
    let bound = |bound: Option<u32>| bound.map_or_else(|| "unknown".to_owned(), |b| b.to_string());
    match TimeSample::parse(&bytes) {
        Err(error) => {
            let _ = writeln!(out, "error {error}");
        }
        Ok(sample) if u64::from(sample.generation) != generation => {
            let _ = writeln!(out, "error generation {}", sample.generation);
        }
        Ok(sample) => {
            let host = &sample.host;
            let _ = writeln!(
                out,
                "ok flags={} generation={} status={} utc_ns={} tsc={} pairing_ns={} \
                 disruption={} correction={} correction_bound={} max_error_us={}",
                sample.flags,
                sample.generation,
                host.status,
                sample.utc_ns,
                sample.tsc,
                sample.pairing_ns,
                host.disruption,
                host.correction,
                bound(host.correction_bound),
                bound(host.max_error_us)
            );
        }
    }
    Ok(())
}

/// Pairs a synthetic time sample's guest TSC and host UTC with synthetic reads of the guest's
/// clocks, and prints the offset and its uncertainty, or why the pairing failed.
fn test_pair(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    let [
        utc_ns,
        tsc,
        pairing_ns,
        before,
        after,
        realtime_ns,
        realtime_tsc,
        half_cycles,
        tsc_hz,
        frequency,
    ] = exactly(arguments)?;
    let sample = TimeSample {
        flags: 0,
        generation: 0,
        utc_ns: number(utc_ns)?,
        tsc: number(tsc)?,
        pairing_ns: number(pairing_ns)?,
        host: decoders::HostClock::default(),
    };
    let reads = GuestReads {
        before: number(before)?,
        after: number(after)?,
        realtime_ns: number(realtime_ns)?,
        realtime_tsc: number(realtime_tsc)?,
        realtime_half_cycles: number(half_cycles)?,
    };
    let rates = ClockRates {
        tsc_hz: number(tsc_hz)?,
        frequency: number(frequency)?,
    };
    match decoders::pair_sample(&sample, &reads, rates) {
        Ok((theta, epsilon)) => writeln!(out, "theta_ns={theta} epsilon_ns={epsilon}"),
        Err(error) => writeln!(out, "error {error}"),
    }
    .ok();
    Ok(())
}

#[derive(Clone, Copy)]
/// Line-oriented decoder selected by the compatibility test.
enum LineMode {
    /// Match runtime kernel-message watch rules.
    Kmsg,
    /// Scan boot-log evidence.
    BootLog,
    /// Strip one syslog record's metadata.
    Syslog,
}

/// Runs one line-oriented decoder over a fixture.
fn test_lines(
    arguments: &[&[u8]],
    mode: LineMode,
    out: &mut dyn Write,
) -> Result<(), CommandError> {
    let [path] = exactly(arguments)?;
    let text = read_fixture(path)?;
    if matches!(mode, LineMode::Kmsg) {
        for line in text
            .split(|&byte| byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            match decoders::match_watch_rule(line) {
                Some(code) => writeln!(out, "{code}"),
                None => writeln!(out, "none"),
            }
            .ok();
        }
        return Ok(());
    }
    let log = decoders::scan_boot_log(&text, matches!(mode, LineMode::Syslog));
    match log.verdict() {
        Ok(()) => {
            let _ = writeln!(out, "ok");
        }
        Err(detail) => {
            let _ = out.write_all(&[b"fail ".as_slice(), &detail, b"\n"].concat());
        }
    }
    if let Some(wx) = &log.wx {
        let _ = out.write_all(&[b"wx ".as_slice(), wx, b"\n"].concat());
    }
    Ok(())
}

/// Reproduces synchronous or asynchronous console byte output.
fn test_console(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    let [line, mode] = exactly(arguments)?;
    if mode == b"async" {
        let _ = out.write_all(b"\n");
    }
    let _ = out.write_all(line);
    Ok(())
}

/// Parses a 32-bit CPUID value as plain decimal or `0x`-prefixed hexadecimal, as `parse_u64`
/// reads the guest programs' arguments.
fn cpuid_value(text: &[u8]) -> Result<u32, CommandError> {
    parse_u64(text)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or(CommandError::Usage)
}

/// Validates one synthetic exhaustive CPUID leaf.
fn test_exhaustive_leaf(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    let [
        leaf,
        eax,
        ebx,
        ecx,
        edx,
        high_eax,
        high_ebx,
        high_ecx,
        high_edx,
    ] = exactly(arguments)?;
    let result = |eax, ebx, ecx, edx| -> Result<CpuidResult, CommandError> {
        Ok(CpuidResult {
            eax: cpuid_value(eax)?,
            ebx: cpuid_value(ebx)?,
            ecx: cpuid_value(ecx)?,
            edx: cpuid_value(edx)?,
        })
    };
    let value = result(eax, ebx, ecx, edx)?;
    let out_of_range = result(high_eax, high_ebx, high_ecx, high_edx)?;
    match decoders::exhaustive_leaf_ok(cpuid_value(leaf)?, value, out_of_range) {
        Ok(()) => {
            let _ = writeln!(out, "pass");
        }
        Err(detail) => {
            let _ = writeln!(out, "fail {detail}");
        }
    }
    Ok(())
}

/// Formats one exhaustive subcheck report.
fn test_exhaustive_report(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    let [check, cpu, status, detail] = exactly(arguments)?;
    let passed = status == b"pass";
    let _ = writeln!(
        out,
        "NVX-TIME-ABI-EXHAUSTIVE: v=1 check={} cpu={} status={} detail=\"{}\"",
        utf8(check)?,
        number::<usize>(cpu)?,
        if passed { "pass" } else { "fail" },
        escape_text(detail, DETAIL_MAX - 1),
    );
    let _ = writeln!(out, "failures={}", usize::from(!passed));
    Ok(())
}

/// Formats one exhaustive final summary.
fn test_exhaustive_summary(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    let [online, failures] = exactly(arguments)?;
    let online: usize = number(online)?;
    let failures: usize = number(failures)?;
    let status = if failures == 0 { "ok" } else { "fail" };
    let _ = writeln!(
        out,
        "NVX-TIME-ABI-EXHAUSTIVE: v=1 status={status} cpus={online} failures={failures}"
    );
    let _ = writeln!(out, "exit={}", i32::from(failures != 0));
    Ok(())
}

/// Parses a CPU list that must name at least one CPU.
fn selected_cpus(text: &[u8]) -> Result<CpuMask, CommandError> {
    CpuMask::parse(text)
        .filter(|cpus| !cpus.is_empty())
        .ok_or(CommandError::Usage)
}

/// Validates a synthetic `/proc/timer_list` fixture.
fn test_timer_list(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    let [path, cpus] = exactly(arguments)?;
    let text = read_fixture(path)?;
    match decoders::timer_list_verdict(&text, selected_cpus(cpus)?) {
        Ok(()) => writeln!(out, "ok"),
        Err(detail) => writeln!(out, "fail {detail}"),
    }
    .ok();
    Ok(())
}

/// Plans and prints one step or slew correction, with the `adjtimex` modes and status that the
/// system layer would apply, for a discipline that holds a frequency correction and adds a host's
/// error.
fn test_correction(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    let [theta, epsilon, speed, frequency, host_error_us] = exactly(arguments)?;
    let correction = decoders::plan_correction(
        number(theta)?,
        number(epsilon)?,
        speed == b"fast",
        number(frequency)?,
        number(host_error_us)?,
    );
    let modes = correction.modes();
    match correction {
        ClockAdjustment::Step {
            seconds,
            nanoseconds,
        } => {
            let _ = writeln!(
                out,
                "step modes=0x{modes:x} sec={seconds} nsec={nanoseconds}"
            );
        }
        ClockAdjustment::Slew {
            offset_ns,
            time_constant,
            hold,
        } => {
            let _ = writeln!(
                out,
                "slew modes=0x{modes:x} offset={offset_ns} status=0x{:x} \
                 constant={time_constant} freq={} maxerror={} esterror={}",
                correction.status(),
                hold.scaled_ppm,
                hold.maximum_error_us,
                hold.estimated_error_us
            );
        }
        ClockAdjustment::Query | ClockAdjustment::Frequency(_) => {}
    }
    Ok(())
}

/// Converts one host clock correction, against the TSC counted at a declared rate, to the
/// frequency correction that the discipline holds, its parts per billion, and whether Linux's
/// limit clamped it.
fn test_frequency(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    let [correction, tsc_hz] = exactly(arguments)?;
    let (frequency, limited) = decoders::guest_frequency(number(correction)?, number(tsc_hz)?);
    let _ = writeln!(
        out,
        "frequency={frequency} ppb={} limited={}",
        decoders::frequency_to_ppb(frequency),
        u8::from(limited)
    );
    Ok(())
}

/// Formats one synthetic time-ABI violation event.
fn test_event(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    let [code, source, phase, generation, boottime_ns, detail] = exactly(arguments)?;
    let phase = match phase {
        b"boot" => Phase::Boot,
        b"capture" => Phase::Capture,
        b"restore" => Phase::Restore,
        _ => Phase::Runtime,
    };
    let line = format_event(
        false,
        utf8(code)?,
        utf8(source)?,
        phase,
        number(generation)?,
        number(boottime_ns)?,
        detail,
    );
    let _ = out.write_all(line.as_bytes());
    Ok(())
}

/// Parses and re-formats one state fixture.
fn test_state(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    let [path] = exactly(arguments)?;
    match TimeState::parse(&read_fixture(path)?) {
        Some(state) => write!(out, "{state}"),
        None => writeln!(out, "error"),
    }
    .ok();
    Ok(())
}

/// Formats status from one state fixture.
fn test_status(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    let [path, mode] = exactly(arguments)?;
    let state = TimeState::parse(&read_fixture(path)?).ok_or(CommandError::Usage)?;
    let _ = out.write_all(state.status_markers(mode == b"report-only").as_bytes());
    let _ = writeln!(out, "ok={}", u8::from(state.checks_ok()));
    Ok(())
}

/// Parses and prints one private restore record, with its CPUs one by one.
fn test_restore_record(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    let [path] = exactly(arguments)?;
    let Some((generation, cpus)) = decoders::parse_restore_record(&read_fixture(path)?) else {
        let _ = writeln!(out, "error");
        return Ok(());
    };
    let listed = if cpus.is_empty() {
        "none".to_owned()
    } else {
        cpus.iter()
            .map(|cpu| cpu.to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    let _ = writeln!(
        out,
        "generation={generation} new_cpus={listed} count={}",
        cpus.len()
    );
    Ok(())
}

/// Formats a report-only conformance failure event.
fn failure_event(id: CheckId, detail: &[u8]) -> String {
    format_event(
        true,
        ViolationCode::Conformance(id),
        EventSource::Conformance,
        Phase::Boot,
        0,
        clock_ns(sys::Clock::Boottime),
        detail,
    )
}

/// Validates and summarizes a synthetic cpuinfo fixture.
fn test_cpuinfo(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    let [path, cpus] = exactly(arguments)?;
    let text = read_fixture(path)?;
    let (cpuinfo, verdict) = decoders::check_cpuinfo(&text, selected_cpus(cpus)?, true);
    match verdict {
        Ok(()) => {
            let _ = write!(out, "ok");
            for (cpu, khz) in cpuinfo.khz.iter().enumerate() {
                if let Some(khz) = khz {
                    let _ = write!(out, " cpu{cpu}_khz={khz}");
                }
            }
            let _ = writeln!(out);
        }
        Err(detail) => {
            let _ = out.write_all(failure_event(CheckId::C5, detail.as_bytes()).as_bytes());
        }
    }
    Ok(())
}

/// Validates a synthetic kernel command line.
fn test_cmdline(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    let [command_line] = exactly(arguments)?;
    match decoders::check_cmdline(command_line) {
        Ok(()) => {
            let _ = writeln!(out, "ok");
        }
        Err(detail) => {
            let _ = out.write_all(failure_event(CheckId::C9, &detail).as_bytes());
        }
    }
    Ok(())
}

/// Validates synthetic current and available clocksources.
fn test_clocksource(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    let [current, available] = exactly(arguments)?;
    match decoders::check_clocksource(current, available) {
        Ok(()) => {
            let _ = writeln!(out, "ok");
        }
        Err(detail) => {
            let _ = out.write_all(failure_event(CheckId::C6, &detail).as_bytes());
        }
    }
    Ok(())
}

/// Exercises the merge of parallel check reports as the C program's `check_cpus_parallel` ran
/// them: the foreground fails once, then three side jobs fail once, twice, and three times, and
/// then each selected CPU's job fails C1 when it cannot run on its CPU.
fn test_parallel(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    let [cpus] = exactly(arguments)?;
    let selected = selected_cpus(cpus)?;
    let mut failures = 0;
    let mut fail = |id: CheckId, detail: &str| {
        let _ = out.write_all(failure_event(id, detail.as_bytes()).as_bytes());
        failures += 1;
    };
    fail(CheckId::C9, "foreground failure");
    for side in 1..=3 {
        for index in 1..=side {
            fail(CheckId::C4, &format!("side job failure {index}"));
        }
    }
    for cpu in selected.iter() {
        let pinned = std::thread::spawn(move || sys::pin_current_thread(cpu))
            .join()
            .unwrap_or(Err(Errno::EINVAL));
        if let Err(error) = pinned {
            fail(
                CheckId::C1,
                &format!("cannot run on cpu {cpu}: {}", error.desc()),
            );
        }
    }
    let _ = writeln!(out, "failures={failures}");
    Ok(())
}

/// A test thread that reports its thread ID and waits until it is told to end.
struct WaitingThread {
    /// The channel whose disconnection ends the thread; it carries nothing.
    end: mpsc::Sender<()>,
    /// The running thread.
    thread: std::thread::JoinHandle<()>,
}

impl WaitingThread {
    /// Starts the thread and returns it with its thread ID.
    fn start() -> Result<(Self, sys::ProcessId), CommandError> {
        let (end, ended) = mpsc::channel();
        let (sender, receiver) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let _ = sender.send(sys::current_thread());
            let _ = ended.recv();
        });
        let id = receiver.recv().map_err(|_| CommandError::Operation)?;
        Ok((Self { end, thread }, id))
    }

    /// Tells the thread to end, and joins it.
    fn end(self) -> Result<(), CommandError> {
        let Self { end, thread } = self;
        drop(end);
        thread.join().map_err(|_| CommandError::Operation)
    }
}

/// Starts deferred work whose promoter promotes it after `delay_ns`, in the monotonic `now`.
fn start_promoter(now: i64, delay_ns: i64) -> Result<Promoter, CommandError> {
    Promoter::start(now.wrapping_sub(IDLE_BOUND_NS).wrapping_add(delay_ns))
        .ok_or(CommandError::Operation)
}

/// Returns the scheduling policy of `thread`, or of the calling thread for `None`, as the C
/// program printed `sched_getscheduler`'s result: the policy's number, or -1 for a failure.
fn printed_policy(thread: Option<sys::ProcessId>) -> i64 {
    sys::scheduling_policy(thread).map_or(-1, i64::from)
}

/// Exercises the promoter: one that promotes an idle caller and the thread that it started,
/// and one that its deferred work stops before its deadline.
fn test_promote(out: &mut dyn Write) -> Result<(), CommandError> {
    let (caller, thread) = {
        let promoter = start_promoter(clock_ns(sys::Clock::Monotonic), PROMOTION_DELAY_NS)?;
        let _idle = sys::IdlePriority::enter();
        let (waiting, id) = WaitingThread::start()?;
        promoter.finish();
        let policies = (printed_policy(None), printed_policy(Some(id)));
        waiting.end()?;
        policies
    };
    let _ = writeln!(out, "promoted caller={caller} thread={thread}");

    let start = clock_ns(sys::Clock::Monotonic);
    let promoter = start_promoter(start, CANCELLED_PROMOTION_DELAY_NS)?;
    let _idle = sys::IdlePriority::enter();
    promoter.stop();
    let _ = writeln!(
        out,
        "stopped caller={} stop_ms={}",
        printed_policy(None),
        clock_ns(sys::Clock::Monotonic).wrapping_sub(start) / NSEC_PER_MSEC
    );
    Ok(())
}

/// Exercises direct idle-to-normal scheduler transitions.
fn test_idle_priority(out: &mut dyn Write) -> Result<(), CommandError> {
    let idle = {
        let _idle = sys::IdlePriority::enter();
        printed_policy(None)
    };
    let _ = writeln!(out, "idle={idle} normal={}", printed_policy(None));
    Ok(())
}

/// Spends `milliseconds` of this process's CPU time.
fn burn_cpu(milliseconds: i64) {
    let deadline = cpu_time_ns().wrapping_add(milliseconds.wrapping_mul(NSEC_PER_MSEC));
    while cpu_time_ns() < deadline {
        std::hint::spin_loop();
    }
}

/// Forks a repair step 7 worker as the daemon does, after `PRE_MS` of preparation, with a fork
/// that costs the parent `FORK_MS`; the worker spends `CHILD_MS` and prints its preparation
/// share and step 7's CPU time.
fn test_deferred_restore_cpu(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    let [preparation_ms, fork_ms, child_ms] = exactly(arguments)?;
    let [preparation_ms, fork_ms, child_ms] = [
        number::<i64>(preparation_ms)?,
        number(fork_ms)?,
        number(child_ms)?,
    ];
    let mut timing = daemon::empty_restore_timing();
    let start = cpu_time_ns();
    burn_cpu(preparation_ms);
    match daemon::fork_deferred_restore(start, &mut timing, || burn_cpu(fork_ms)) {
        Ok(ForkResult::Child) => {
            burn_cpu(child_ms);
            let line = format!(
                "prior_ms={} total_ms={}\n",
                timing.prior_cpu_ns / NSEC_PER_MSEC,
                daemon::deferred_restore_cpu_ns(&timing) / NSEC_PER_MSEC
            );
            let _ = out.write_all(line.as_bytes());
            sys::exit_immediately(0)
        }
        Ok(ForkResult::Parent(process)) => match sys::wait(process) {
            Some(ChildStatus::Exited(0)) => Ok(()),
            _ => Err(CommandError::Operation),
        },
        Err(_) => Err(CommandError::Operation),
    }
}

/// Checks, in a VM, that a process that `nvx-time` starts after a port transaction cannot reach
/// OpenVMM's ports. It takes a time sample under the port-B lock, as the daemon does, whose
/// transaction ends before the sample returns, and then starts `port-probe` twice for each
/// probed port, as the time tools start a process: once to take the permission itself, which
/// must read the port, and once without it, which the processor must refuse. Prints `denied`,
/// or the first unexpected result.
fn test_port_access(out: &mut dyn Write) -> Result<(), CommandError> {
    sys::reset_child_signal();
    let runtime = Runtime::new(&system::RealOps, true);
    // The time daemon samples port B too, so the sample holds the lock that serializes port-B
    // transactions, and gives it up before the probes start.
    let sample = {
        let _lock = match system::lock_portb() {
            Ok(lock) => lock,
            Err(error) => {
                let detail = system::portb_lock_failure(error);
                let _ = writeln!(out, "{}", String::from_utf8_lossy(&detail));
                return Ok(());
            }
        };
        system::best_sample(&runtime, 1, i64::MAX, 0)
    };
    if let Err(SampleError::Ports(error)) = sample {
        let _ = writeln!(out, "the transaction failed: {error}");
        return Ok(());
    }
    for (name, _) in PROBED_PORTS {
        let granted = probe(name, true)?;
        if granted != ChildStatus::Exited(0) {
            let _ = writeln!(
                out,
                "{} with the permission: {granted:?}",
                name.to_string_lossy()
            );
            return Ok(());
        }
        let inherited = probe(name, false)?;
        if inherited != ChildStatus::Signaled(Signal::SIGSEGV as i32) {
            let _ = writeln!(
                out,
                "{} without the permission: {inherited:?}",
                name.to_string_lossy()
            );
            return Ok(());
        }
    }
    let _ = writeln!(out, "denied");
    Ok(())
}

/// Starts `nvx-time test port-probe NAME`, with `grant` when the probe takes the permission
/// itself, and returns how it ended.
fn probe(name: &CStr, grant: bool) -> Result<ChildStatus, CommandError> {
    let arguments: Vec<CString> = [c"nvx-time", c"test", c"port-probe", name]
        .into_iter()
        .chain(grant.then_some(c"grant"))
        .map(CStr::to_owned)
        .collect();
    match system::fork_process() {
        Ok(ForkResult::Child) => {
            sys::exec(c"/proc/self/exe", &arguments, None);
            sys::exit_immediately(127)
        }
        Ok(ForkResult::Parent(process)) => sys::wait(process).ok_or(CommandError::Operation),
        Err(_) => Err(CommandError::Operation),
    }
}

/// Reads the port that `port-access` names, without the permission unless `grant` follows, in
/// which case it takes the permission first. Without the permission, the processor refuses the
/// read, and Linux ends the process with `SIGSEGV`.
fn test_port_probe(arguments: &[&[u8]]) -> Result<(), CommandError> {
    let (name, grant) = match arguments {
        [name] => (*name, false),
        [name, b"grant"] => (*name, true),
        _ => return Err(CommandError::Usage),
    };
    let (_, port) = PROBED_PORTS
        .into_iter()
        .find(|(probed, _)| probed.to_bytes() == name)
        .ok_or(CommandError::Usage)?;
    // The probe holds the permission that it takes until it returns.
    let _ports = if grant {
        Some(PortAccess::request().map_err(|_| CommandError::Operation)?)
    } else {
        None
    };
    sys::probe_port(port);
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Focused regressions for the conversions of the test hooks.

    use super::*;

    fn hook(arguments: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        run(arguments, &mut out).unwrap();
        out
    }

    #[test]
    fn cpuid_values_are_32_bit_decimal_or_hexadecimal() {
        assert_eq!(cpuid_value(b"0x40000006"), Ok(0x4000_0006));
        assert_eq!(cpuid_value(b"7"), Ok(7));
        assert_eq!(cpuid_value(b"4294967295"), Ok(u32::MAX));
        for text in [&b"4294967296"[..], b"-1", b"x", b""] {
            assert_eq!(cpuid_value(text), Err(CommandError::Usage), "{text:?}");
        }
    }

    #[test]
    fn malformed_hooks_fail_with_the_usage_status() {
        /// The arguments of a well-formed `pair` hook.
        const PAIR: [&[u8]; 11] = [
            b"pair",
            b"1000000000000000000",
            b"1000000",
            b"500",
            b"999000",
            b"1002000",
            b"999999999999000000",
            b"1004000",
            b"40",
            b"2000000000",
            b"0",
        ];
        let mut out = Vec::new();
        assert_eq!(run(&[], &mut out), Err(CommandError::Usage));
        assert_eq!(run(&[b"unknown"], &mut out), Err(CommandError::Usage));
        assert_eq!(run(&[b"pair", b"1"], &mut out), Err(CommandError::Usage));
        // Time ABI 1's pairing of a realtime bracket is no longer a hook.
        assert_eq!(
            run(&[b"pair", b"1000", b"3000", b"5000"], &mut out),
            Err(CommandError::Usage)
        );
        let mut plus = PAIR;
        plus[2] = b"+1000000";
        assert_eq!(run(&plus, &mut out), Err(CommandError::Usage));
        let mut negative = PAIR;
        negative[3] = b"-500";
        assert_eq!(run(&negative, &mut out), Err(CommandError::Usage));
        assert_eq!(
            run(&[b"frequency", b"-40000000"], &mut out),
            Err(CommandError::Usage)
        );
        assert_eq!(
            run(
                &[b"frequency", b"9223372036854775808", b"2100000000"],
                &mut out
            ),
            Err(CommandError::Usage)
        );
        assert_eq!(
            run(&[b"frequency", b"0", b"-1"], &mut out),
            Err(CommandError::Usage)
        );
        assert_eq!(
            run(&[b"correction", b"-5", b"10", b"slow"], &mut out),
            Err(CommandError::Usage)
        );
        assert_eq!(
            run(&[b"deferral", b"x"], &mut out),
            Err(CommandError::Usage)
        );
        // Only malformed: a well-formed probe without the permission ends the process.
        for arguments in [
            &[b"port-access".as_slice(), b"x"][..],
            &[b"port-probe"],
            &[b"port-probe", b"control"],
            &[b"port-probe", b"snapshot", b"revoke"],
            &[b"port-probe", b"snapshot", b"grant", b"x"],
        ] {
            assert_eq!(run(arguments, &mut out), Err(CommandError::Usage));
        }
        assert_eq!(
            run(
                &[b"exhaustive-report", b"X\xff", b"0", b"pass", b"ok"],
                &mut out
            ),
            Err(CommandError::Usage)
        );
        assert_eq!(
            run(
                &[
                    b"event", b"G_\xff", b"watcher", b"runtime", b"0", b"1", b"x"
                ],
                &mut out
            ),
            Err(CommandError::Usage)
        );
        assert!(out.is_empty());
        assert_eq!(run(&PAIR, &mut out), Ok(()));
        assert_eq!(out, b"theta_ns=1002000 epsilon_ns=1524\n");
    }

    #[test]
    fn asynchronous_console_lines_are_written_whole() {
        let line = [b'a'; 700];
        let mut out = Vec::new();
        run(&[b"console", &line, b"async"], &mut out).unwrap();
        assert_eq!(out, [b"\n".as_slice(), &line].concat());
    }

    #[test]
    fn console_lines_preserve_synchronous_and_asynchronous_boundaries() {
        let line = b"NVX-TIME-ABI-VIOLATION: v=1 code=G_RCU_STALL source=watcher\n";
        assert_eq!(hook(&[b"console", line, b"sync"]), line);
        assert_eq!(
            hook(&[b"console", line, b"async"]),
            [b"\n".as_slice(), line].concat()
        );
    }

    #[test]
    fn exhaustive_hooks_preserve_leaf_rules_and_report_formats() {
        let out_of_range: [&[u8]; 4] = [b"0x00000d0b", b"0x00000001", b"0x00000002", b"0x00000003"];
        let zero: [&[u8]; 4] = [b"0", b"0", b"0", b"0"];
        let leaf = |number: &[u8], value: [&[u8]; 4]| {
            hook(&[
                b"exhaustive-leaf",
                number,
                value[0],
                value[1],
                value[2],
                value[3],
                out_of_range[0],
                out_of_range[1],
                out_of_range[2],
                out_of_range[3],
            ])
        };
        assert_eq!(leaf(b"0x40000006", zero), b"pass\n");
        assert_eq!(leaf(b"0x40000010", out_of_range), b"pass\n");
        assert_eq!(leaf(b"0x40000100", zero), b"pass\n");
        assert_eq!(leaf(b"0x40000100", out_of_range), b"pass\n");

        let explicit = String::from_utf8(leaf(b"0x40000081", out_of_range)).unwrap();
        assert!(
            explicit.starts_with("fail leaf 0x40000081 is "),
            "{explicit}"
        );
        assert!(explicit.ends_with("expected zero\n"), "{explicit}");
        let kvm = [
            b"0x40000101".as_slice(),
            b"0x4b4d564b",
            b"0x564b4d56",
            b"0x0000004d",
        ];
        let signature = String::from_utf8(leaf(b"0x40000100", kvm)).unwrap();
        assert!(
            signature.contains("(signature \"KVMKVMKVM...\")"),
            "{signature}"
        );
        assert!(
            signature.ends_with("or the highest basic leaf\n"),
            "{signature}"
        );

        assert_eq!(
            hook(&[b"exhaustive-report", b"X3", b"2", b"pass", b"ok"]),
            concat!(
                "NVX-TIME-ABI-EXHAUSTIVE: v=1 check=X3 cpu=2 status=pass detail=\"ok\"\n",
                "failures=0\n"
            )
            .as_bytes()
        );
        assert_eq!(
            hook(&[
                b"exhaustive-report",
                b"X2",
                b"7",
                b"fail",
                b"base 0x40000100 \"KVMKVMKVM\" \\ x\ty",
            ]),
            concat!(
                "NVX-TIME-ABI-EXHAUSTIVE: v=1 check=X2 cpu=7 status=fail ",
                "detail=\"base 0x40000100 \\\"KVMKVMKVM\\\" \\\\ x\\x09y\"\n",
                "failures=1\n"
            )
            .as_bytes()
        );
        assert_eq!(
            hook(&[b"exhaustive-summary", b"4", b"0"]),
            concat!(
                "NVX-TIME-ABI-EXHAUSTIVE: v=1 status=ok cpus=4 failures=0\n",
                "exit=0\n"
            )
            .as_bytes()
        );
        assert_eq!(
            hook(&[b"exhaustive-summary", b"8", b"3"]),
            concat!(
                "NVX-TIME-ABI-EXHAUSTIVE: v=1 status=fail cpus=8 failures=3\n",
                "exit=1\n"
            )
            .as_bytes()
        );
    }

    #[test]
    fn accepted_cmdline_and_clocksource_hooks_print_ok() {
        assert_eq!(
            hook(&[
                b"cmdline",
                b"earlycon=xe9 console=hvc0 quiet clocksource=tsc tsc=reliable",
            ]),
            b"ok\n"
        );
        assert_eq!(hook(&[b"clocksource", b"tsc\n", b"tsc \n"]), b"ok\n");
    }

    #[test]
    fn corrections_print_the_kernel_modes() {
        let mut out = Vec::new();
        run(
            &[b"correction", b"128000000", b"10", b"fast", b"0", b"0"],
            &mut out,
        )
        .unwrap();
        run(
            &[b"correction", b"-5", b"10", b"slow", b"65536", b"7"],
            &mut out,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "step modes=0x2100 sec=0 nsec=128000000\n\
             slew modes=0x203f offset=-5 status=0x2081 constant=6 freq=65536 maxerror=8 \
             esterror=1\n"
        );
    }

    #[test]
    fn ported_corrections_print_exact_results() {
        let mut out = Vec::new();
        for arguments in [
            [&b"127999999"[..], b"10002", b"fast"],
            [b"-500", b"20", b"slow"],
            [b"128000000", b"10", b"fast"],
            [b"-1128000000", b"10", b"slow"],
        ] {
            let hook = [&[&b"correction"[..]][..], &arguments, &[b"0", b"0"]].concat();
            run(&hook, &mut out).unwrap();
        }
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "slew modes=0x203f offset=127999999 status=0x2081 constant=4 freq=0 \
             maxerror=128011 esterror=11\n\
             slew modes=0x203f offset=-500 status=0x2081 constant=6 freq=0 maxerror=1 \
             esterror=1\n\
             step modes=0x2100 sec=0 nsec=128000000\n\
             step modes=0x2100 sec=-2 nsec=872000000\n"
        );
    }

    #[test]
    fn pairing_and_frequency_hooks_preserve_signed_arithmetic() {
        let pair = |tsc: &[u8]| {
            hook(&[
                b"pair",
                b"1000000000000000000",
                tsc,
                b"500",
                b"999000",
                b"1002000",
                b"999999999999000000",
                b"1004000",
                b"40",
                b"2000000000",
                b"0",
            ])
        };
        assert_eq!(pair(b"1000000"), b"theta_ns=1002000 epsilon_ns=1524\n");
        assert_eq!(
            pair(b"995999"),
            b"error time sample TSC 995999 is outside the guest's reads 999000 to 1002000\n"
        );
        for (correction, tsc_hz, expected) in [
            (
                b"13107200".as_slice(),
                b"2100000000".as_slice(),
                b"frequency=13107200 ppb=200000 limited=0\n".as_slice(),
            ),
            (
                b"39321600",
                b"2100000000",
                b"frequency=32768000 ppb=500000 limited=1\n",
            ),
            (b"0", b"2194843123", b"frequency=-3673 ppb=-56 limited=0\n"),
            (
                b"-65536",
                b"2000000000",
                b"frequency=-65536 ppb=-1000 limited=0\n",
            ),
        ] {
            assert_eq!(hook(&[b"frequency", correction, tsc_hz]), expected);
        }
    }
}
