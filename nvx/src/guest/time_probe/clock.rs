//! Clock, CPU, hypervisor, kernel-log, timer, and optional MSR diagnostics.

use std::ffi::CStr;
use std::io::Write;
use std::time::Duration;

use super::format::{f0, f9, utc_seconds};
use super::support::{
    Arguments, NANOSECONDS_PER_SECOND, cpuinfo_has_flag, current_clocksource, detect_tsc_rate,
    kmsg_foreach, measure_tsc_hz, parse_rate, round_ll,
};
use crate::guest::kernel_text::{self, ClockEventState, TickDevice, TickMode, TimerList};
use crate::guest::parse::parse_u64;
use crate::guest::status::CommandError;
use crate::guest::sys::x86::{cpuid, msr};
use crate::guest::sys::{self, Errno, MsrAccess, MsrFile, kernel};

/// Milliseconds over which the clock probe measures the TSC rate without `--measure-ms`.
const DEFAULT_MEASURE_MS: u64 = 200;
/// The most milliseconds that `--measure-ms` accepts.
const MEASURE_LIMIT_MS: u64 = 60_000;
/// The last hypervisor CPUID leaf that the probe prints.
const LAST_PRINTED_HYPERVISOR_LEAF: u32 = 0x4000_0010;
/// Hyper-V's vendor signature in CPUID leaf 0x40000000.
const HYPERV_VENDOR: &str = "Microsoft Hv";
/// The MSRs that `--msr` prints, with their architectural names.
const PRINTED_MSRS: [(u32, &str); 7] = [
    (msr::IA32_TSC, "IA32_TSC"),
    (msr::IA32_TSC_ADJUST, "IA32_TSC_ADJUST"),
    (msr::HV_VP_INDEX, "HV_X64_MSR_VP_INDEX"),
    (msr::HV_TSC_FREQUENCY, "HV_X64_MSR_TSC_FREQUENCY"),
    (msr::HV_APIC_FREQUENCY, "HV_X64_MSR_APIC_FREQUENCY"),
    (
        msr::HV_TSC_INVARIANT_CONTROL,
        "HV_X64_MSR_TSC_INVARIANT_CONTROL",
    ),
    (msr::KVM_SYSTEM_TIME_NEW, "MSR_KVM_SYSTEM_TIME_NEW"),
];

/// Returns bit `bit` of `register` as the 0 or 1 that the probe prints.
fn bit(register: u32, bit: u32) -> u8 {
    u8::from(cpuid::bit(register, bit))
}

/// CPU flags whose presence is summarized in probe output.
const TSC_FLAGS: &[&str] = &[
    "tsc",
    "rdtscp",
    "constant_tsc",
    "nonstop_tsc",
    "nonstop_tsc_s3",
    "tsc_known_freq",
    "tsc_reliable",
    "tsc_deadline_timer",
    "tsc_adjust",
    "art",
    "hypervisor",
    "arat",
    "aperfmperf",
    "hwp",
    "x2apic",
];
/// Kernel-message substrings identifying the active hypervisor.
const HYPERVISOR_PATTERNS: &[&[u8]] = &[
    b"Hypervisor detected",
    b"Hyper-V",
    b"Booting paravirtualized kernel",
    b"kvm-guest",
];
/// Kernel-message substrings relevant to clocks and stalls.
const TIME_PATTERNS: &[&[u8]] = &[
    b"tsc",
    b"TSC",
    b"clocksource",
    b"clockevent",
    b"kvm-clock",
    b"APIC timer",
    b"LAPIC",
    b"lapic",
    b"calibrat",
    b"Calibrat",
    b"unchecked MSR",
    b"hpet",
    b"HPET",
    b"timekeeping",
    b"sched_clock",
    b"detected stall",
    b"watchdog",
    b"lockup",
    b"hung_task",
    b"jiffies",
    b"rtc",
    b"RTC",
];
/// Kernel time/watchdog controls printed by the clock probe.
const PARAM_FILES: [&CStr; 7] = [
    kernel::RCU_STALL_SUPPRESS,
    kernel::RCU_STALL_TIMEOUT,
    kernel::WATCHDOG,
    kernel::SOFT_WATCHDOG,
    kernel::NMI_WATCHDOG,
    kernel::WATCHDOG_THRESHOLD,
    kernel::HUNG_TASK_TIMEOUT,
];

/// Appends `item` to `list` unless the list holds it already.
fn add_unique<T: PartialEq>(list: &mut Vec<T>, item: T) {
    if !list.contains(&item) {
        list.push(item);
    }
}

/// Returns `list`'s items separated by commas, or `empty` for an empty list.
fn joined<T: AsRef<str>>(list: &[T], empty: &str) -> String {
    if list.is_empty() {
        empty.to_owned()
    } else {
        list.iter().map(AsRef::as_ref).collect::<Vec<_>>().join(",")
    }
}

/// Returns `text` with each character unsafe for a one-token marker replaced by an underscore.
fn sanitize_token(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '#' | '-' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// Reads and prints one named clock, including UTC for realtime.
fn print_clock(output: &mut dyn Write, name: &str, clock: sys::Clock) -> Option<sys::TimeSpec> {
    let time = match sys::clock_time(clock) {
        Ok(time) => time,
        Err(error) => {
            let _ = writeln!(output, "{name:<20} unavailable ({})", error.desc());
            return None;
        }
    };
    let _ = write!(output, "{name:<20} {}.{:09}", time.tv_sec(), time.tv_nsec());
    if clock == sys::Clock::Realtime {
        let _ = write!(
            output,
            "  ({}.{:09}Z)",
            utc_seconds(time.tv_sec()),
            time.tv_nsec()
        );
    }
    let _ = writeln!(output);
    Some(time)
}

#[derive(Default)]
/// Counters accumulated while filtering kernel messages.
struct Filter {
    /// Number of messages printed for the requested patterns.
    matches: usize,
    /// Number of TSC-instability messages.
    tsc_unstable: usize,
    /// Number of unchecked-MSR messages.
    unchecked_msr: usize,
}

/// Prints matching kernel messages and updates diagnostic counters.
fn print_kmsg(
    output: &mut dyn Write,
    patterns: &[&[u8]],
    filter: &mut Filter,
) -> Result<(), Errno> {
    kmsg_foreach(|record| {
        let text = record.message;
        if kernel_text::contains(text, b"TSC unstable") {
            filter.tsc_unstable += 1;
        }
        if kernel_text::contains(text, b"unchecked MSR access") {
            filter.unchecked_msr += 1;
        }
        if patterns
            .iter()
            .any(|pattern| kernel_text::contains(text, pattern))
        {
            let usec = record.timestamp_us.unwrap_or(0);
            let _ = write!(output, "[{:5}.{:06}] ", usec / 1_000_000, usec % 1_000_000);
            let _ = output.write_all(text);
            let _ = output.write_all(b"\n");
            filter.matches += 1;
        }
    })
}

/// Returns the name of `device`'s clock-event device, or `none`.
fn device_name(device: &TickDevice) -> &str {
    device.device.as_deref().unwrap_or("none")
}

/// Returns the name of `device`'s event handler, or `none`.
fn handler_name(device: &TickDevice) -> &str {
    device.handler.as_deref().unwrap_or("none")
}

/// Returns the name of `device`'s tick mode, or `unknown`.
fn tick_mode_name(device: &TickDevice) -> &'static str {
    device.tick_mode().map_or("unknown", TickMode::name)
}

/// Returns the name of `device`'s clock-event state, or `unknown`.
fn state_name(device: &TickDevice) -> &'static str {
    device
        .clock_event_state()
        .map_or("unknown", ClockEventState::name)
}

/// Formats C `%#x`, which prefixes only nonzero values with `0x`.
fn alternate_hex(value: u64) -> String {
    if value == 0 {
        "0".to_owned()
    } else {
        format!("{value:#x}")
    }
}

/// Formats C `%#010x`, including its alternate prefix and width.
fn alternate_hex_width_10(value: u32) -> String {
    if value == 0 {
        "0000000000".to_owned()
    } else {
        format!("{value:#010x}")
    }
}

/// Prints timer-list sections and returns the one-line marker summary.
fn print_timer_list(output: &mut dyn Write) -> String {
    let text = match sys::read_file(kernel::TIMER_LIST) {
        Ok(text) => text,
        Err(error) => {
            let _ = writeln!(output, "timer_list: unavailable ({})", error.desc());
            return "clockevents=unavailable".to_owned();
        }
    };
    let list = TimerList::parse(&text);
    if let Some(broadcast) = &list.broadcast {
        let _ = writeln!(
            output,
            "broadcast: device={} state={} tick_mode={} handler={}",
            device_name(broadcast),
            state_name(broadcast),
            tick_mode_name(broadcast),
            handler_name(broadcast)
        );
    }
    let mut devices = Vec::new();
    let mut modes = Vec::new();
    let mut states = Vec::new();
    for (cpu, device) in &list.tick_devices {
        let base = list.timer_bases.get(cpu).copied().unwrap_or_default();
        let _ = writeln!(
            output,
            "cpu{cpu}: device={} state={} tick_mode={} handler={} hres_active={} nohz={}",
            device_name(device),
            state_name(device),
            tick_mode_name(device),
            handler_name(device),
            base.hres_active.unwrap_or(-1),
            base.nohz.unwrap_or(-1)
        );
        add_unique(&mut devices, device_name(device));
        add_unique(&mut modes, tick_mode_name(device));
        add_unique(&mut states, state_name(device));
    }
    if list.broadcast.is_none() && list.tick_devices.is_empty() {
        let _ = writeln!(
            output,
            "timer_list: no tick devices found (file empty or masked)"
        );
    }
    format!(
        "clockevent_cpus={} clockevent_devices={} tick_modes={} clockevent_states={} \
         broadcast={}:{}",
        list.tick_devices.len(),
        joined(&devices, "none"),
        joined(&modes, "none"),
        joined(&states, "none"),
        list.broadcast
            .as_ref()
            .and_then(|broadcast| broadcast.device.as_deref())
            .unwrap_or("none"),
        list.broadcast.as_ref().map_or("none", state_name)
    )
}

#[derive(Default)]
/// CPUID values reused by the final clock-probe marker.
struct CpuidSummary {
    /// Hypervisor vendor signature, or `none`.
    vendor: String,
    /// Maximum Hyper-V leaf.
    max_leaf: u32,
    /// Hyper-V feature EAX.
    features_eax: u32,
    /// Hyper-V feature EDX.
    features_edx: u32,
    /// Whether CPUID reports a hypervisor.
    hypervisor: bool,
}

impl CpuidSummary {
    /// Prints the hypervisor's maximum leaf and vendor signature.
    fn print_hypervisor_leaf(&self, output: &mut dyn Write) {
        let _ = writeln!(
            output,
            "cpuid.0x40000000: eax.max_leaf={} vendor=\"{}\"",
            alternate_hex(u64::from(self.max_leaf)),
            self.vendor
        );
    }

    /// Formats the hypervisor fields of the final clock-probe marker.
    fn hypervisor_marker_fields(&self) -> String {
        format!(
            "hv_vendor={} hv_max_leaf={} hv_features_eax={} hv_features_edx={}",
            sanitize_token(&self.vendor),
            alternate_hex(u64::from(self.max_leaf)),
            alternate_hex(u64::from(self.features_eax)),
            alternate_hex(u64::from(self.features_edx))
        )
    }
}

/// Prints architectural and hypervisor CPUID leaves.
fn print_cpuid(output: &mut dyn Write) -> CpuidSummary {
    let basic = sys::x86::cpuid(cpuid::LEAF_MAXIMUM, 0).eax;
    let extended = sys::x86::cpuid(cpuid::LEAF_EXTENDED_MAXIMUM, 0).eax;
    let leaf1 = sys::x86::cpuid(cpuid::LEAF_FEATURES, 0);
    let signature = cpuid::Signature::decode(leaf1.eax);
    let mut summary = CpuidSummary {
        hypervisor: cpuid::bit(leaf1.ecx, cpuid::FEATURES_ECX_HYPERVISOR),
        ..CpuidSummary::default()
    };
    let _ = writeln!(
        output,
        "cpuid.1: family={} model={} stepping={} edx.tsc={} ecx.tsc_deadline={} \
         ecx.hypervisor={}",
        signature.family,
        signature.model,
        signature.stepping,
        bit(leaf1.edx, cpuid::FEATURES_EDX_TSC),
        bit(leaf1.ecx, cpuid::FEATURES_ECX_TSC_DEADLINE),
        u8::from(summary.hypervisor)
    );
    if basic >= cpuid::LEAF_POWER_MANAGEMENT {
        let value = sys::x86::cpuid(cpuid::LEAF_POWER_MANAGEMENT, 0);
        let _ = writeln!(
            output,
            "cpuid.6: eax.arat={} eax.hwp={} ecx.aperfmperf={}",
            bit(value.eax, cpuid::POWER_MANAGEMENT_EAX_ARAT),
            bit(value.eax, cpuid::POWER_MANAGEMENT_EAX_HWP),
            bit(value.ecx, cpuid::POWER_MANAGEMENT_ECX_APERFMPERF)
        );
    }
    if basic >= cpuid::LEAF_STRUCTURED_FEATURES {
        let value = sys::x86::cpuid(cpuid::LEAF_STRUCTURED_FEATURES, 0);
        let _ = writeln!(
            output,
            "cpuid.7.0: ebx.tsc_adjust={}",
            bit(value.ebx, cpuid::STRUCTURED_FEATURES_EBX_TSC_ADJUST)
        );
    }
    if basic >= cpuid::LEAF_PERFORMANCE_MONITORING {
        let value = sys::x86::cpuid(cpuid::LEAF_PERFORMANCE_MONITORING, 0);
        let _ = writeln!(
            output,
            "cpuid.0xa: eax={} (pmu_version={})",
            alternate_hex_width_10(value.eax),
            value.eax & cpuid::PERFORMANCE_MONITORING_VERSION_MASK
        );
    }
    if basic >= cpuid::LEAF_TSC_CRYSTAL {
        let value = sys::x86::cpuid(cpuid::LEAF_TSC_CRYSTAL, 0);
        let _ = write!(
            output,
            "cpuid.0x15: eax.denominator={} ebx.numerator={} ecx.crystal_hz={}",
            value.eax, value.ebx, value.ecx
        );
        if value.eax != 0 && value.ebx != 0 && value.ecx != 0 {
            let _ = write!(
                output,
                " (tsc_hz={})",
                f0(f64::from(value.ecx) * f64::from(value.ebx) / f64::from(value.eax))
            );
        }
        let _ = writeln!(output);
    }
    if basic >= cpuid::LEAF_FREQUENCY {
        let value = sys::x86::cpuid(cpuid::LEAF_FREQUENCY, 0);
        let _ = writeln!(
            output,
            "cpuid.0x16: eax.base_mhz={} ebx.max_mhz={} ecx.bus_mhz={}",
            value.eax & cpuid::FREQUENCY_MHZ_MASK,
            value.ebx & cpuid::FREQUENCY_MHZ_MASK,
            value.ecx & cpuid::FREQUENCY_MHZ_MASK
        );
    }
    if extended >= cpuid::LEAF_EXTENDED_FEATURES {
        let value = sys::x86::cpuid(cpuid::LEAF_EXTENDED_FEATURES, 0);
        let _ = writeln!(
            output,
            "cpuid.0x80000001: edx.rdtscp={}",
            bit(value.edx, cpuid::EXTENDED_FEATURES_EDX_RDTSCP)
        );
    }
    if extended >= cpuid::LEAF_ADVANCED_POWER {
        let value = sys::x86::cpuid(cpuid::LEAF_ADVANCED_POWER, 0);
        let _ = writeln!(
            output,
            "cpuid.0x80000007: edx.invariant_tsc={}",
            bit(value.edx, cpuid::ADVANCED_POWER_EDX_INVARIANT_TSC)
        );
    }
    if !summary.hypervisor {
        let _ = writeln!(
            output,
            "cpuid.0x40000000: not queried (hypervisor bit clear)"
        );
        summary.vendor = "none".to_owned();
        return summary;
    }
    let value = sys::x86::cpuid(cpuid::LEAF_HYPERVISOR, 0);
    summary.max_leaf = value.eax;
    summary.vendor = cpuid::signature(value);
    summary.print_hypervisor_leaf(output);
    let last = summary.max_leaf.min(LAST_PRINTED_HYPERVISOR_LEAF);
    for leaf in cpuid::LEAF_HYPERVISOR + 1..=last {
        let value = sys::x86::cpuid(leaf, 0);
        let _ = writeln!(
            output,
            "cpuid.{leaf:#x}: eax={} ebx={} ecx={} edx={}",
            alternate_hex_width_10(value.eax),
            alternate_hex_width_10(value.ebx),
            alternate_hex_width_10(value.ecx),
            alternate_hex_width_10(value.edx)
        );
        if leaf == cpuid::LEAF_HYPERV_FEATURES {
            summary.features_eax = value.eax;
            summary.features_edx = value.edx;
        }
    }
    if summary.vendor == HYPERV_VENDOR && summary.max_leaf >= cpuid::LEAF_HYPERV_FEATURES {
        let _ = writeln!(
            output,
            "hyperv.features: hypercall={} vp_index={} access_frequency_msrs={} \
             access_tsc_invariant={} frequency_msrs_available={}",
            bit(summary.features_eax, cpuid::HYPERV_EAX_HYPERCALL),
            bit(summary.features_eax, cpuid::HYPERV_EAX_VP_INDEX),
            bit(summary.features_eax, cpuid::HYPERV_EAX_ACCESS_FREQUENCY),
            bit(summary.features_eax, cpuid::HYPERV_EAX_ACCESS_TSC_INVARIANT),
            bit(summary.features_edx, cpuid::HYPERV_EDX_FREQUENCY_MSRS)
        );
    }
    summary
}

/// Prints one successfully read MSR, including its hexadecimal and decimal values.
fn print_msr(output: &mut dyn Write, index: u32, name: &str, value: u64) {
    let _ = writeln!(
        output,
        "msr {index:#010x} {name:<34} = {} ({value})",
        alternate_hex(value)
    );
}

/// Prints optional CPU-zero MSRs, reporting `EIO` as a trapped `#GP`.
fn print_msrs(output: &mut dyn Write) {
    let device = match MsrFile::open(0, MsrAccess::Read) {
        Ok(device) => device,
        Err(error) => {
            let _ = writeln!(
                output,
                "msr: {} unavailable ({})",
                MsrFile::path(0),
                error.desc()
            );
            return;
        }
    };
    for (index, name) in PRINTED_MSRS {
        match device.read(index) {
            Ok(value) => print_msr(output, index, name, value),
            Err(error) => {
                let reason = if error == Errno::EIO {
                    "read faulted (#GP)"
                } else {
                    error.desc()
                };
                let _ = writeln!(output, "msr {index:#010x} {name:<34} : {reason}");
            }
        }
    }
}

/// Prints processor count, model, and observed MHz values from cpuinfo.
fn cpuinfo_summary(output: &mut dyn Write) {
    let text = sys::read_file(kernel::CPU_INFORMATION).unwrap_or_default();
    let mut processors = 0;
    let mut model_printed = false;
    let mut mhz = Vec::new();
    for line in text.split(|&byte| byte == b'\n') {
        let colon = line.iter().position(|&byte| byte == b':');
        if line.starts_with(b"processor") {
            processors += 1;
        } else if !model_printed && line.starts_with(b"model name") {
            if let Some(colon) = colon {
                let _ = write!(output, "model name:");
                let _ = output.write_all(&line[colon + 1..]);
                let _ = writeln!(output);
                model_printed = true;
            }
        } else if line.starts_with(b"cpu MHz")
            && let Some(colon) = colon
        {
            let start = (colon + 2).min(line.len());
            add_unique(&mut mhz, String::from_utf8_lossy(&line[start..]));
        }
    }
    let _ = writeln!(
        output,
        "processors: {processors}\ncpu MHz: {}",
        joined(&mhz, "unknown")
    );
}

/// Parses options and prints the complete clock diagnostic report.
pub(super) fn run(
    arguments: &[&[u8]],
    output: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    let mut measure_ms = DEFAULT_MEASURE_MS;
    let mut override_hz = None;
    let mut read_msrs = false;
    let mut arguments = Arguments::new(b"clock", arguments);
    while let Some(argument) = arguments.next() {
        if let Some(value) = arguments.value(argument, b"--measure-ms") {
            measure_ms = parse_u64(value)
                .filter(|value| *value <= MEASURE_LIMIT_MS)
                .ok_or_else(|| arguments.invalid(err))?;
        } else if let Some(value) = arguments.value(argument, b"--tsc-hz") {
            override_hz = Some(parse_rate(value).ok_or_else(|| arguments.invalid(err))?);
        } else if argument == b"--msr" {
            read_msrs = true;
        } else {
            return Err(arguments.invalid(err));
        }
    }
    let rate = detect_tsc_rate(override_hz);
    let _ = writeln!(output, "nvx-time-probe {} clock\n== clocks", super::VERSION);
    // The C probe printed -1 for a CPU that `sched_getcpu` could not report.
    let cpu = sys::current_cpu().map_or(-1, |cpu| cpu as i64);
    let tsc = sys::x86::rdtsc_fenced();
    print_clock(output, "CLOCK_REALTIME", sys::Clock::Realtime);
    let monotonic = print_clock(output, "CLOCK_MONOTONIC", sys::Clock::Monotonic)
        .unwrap_or(sys::TimeSpec::new(0, 0));
    print_clock(output, "CLOCK_MONOTONIC_RAW", sys::Clock::MonotonicRaw);
    print_clock(output, "CLOCK_BOOTTIME", sys::Clock::Boottime);
    let _ = write!(output, "{:<20} {tsc} on cpu {cpu}", "TSC");
    if rate.hz > 0.0 {
        let seconds = tsc as f64 / rate.hz;
        let difference = tsc as f64 * NANOSECONDS_PER_SECOND / rate.hz
            - (monotonic.tv_sec() as f64 * NANOSECONDS_PER_SECOND + monotonic.tv_nsec() as f64);
        let _ = write!(
            output,
            " ({} s; tsc_minus_monotonic_ns={})",
            f9(seconds),
            round_ll(difference)
        );
    }
    let _ = writeln!(
        output,
        "\ntsc_hz={} tsc_hz_source={}",
        f0(rate.hz),
        rate.source
    );
    if measure_ms > 0 {
        let _ = writeln!(
            output,
            "tsc_hz_measured={} (over {measure_ms} ms against CLOCK_MONOTONIC_RAW)",
            f0(measure_tsc_hz(Duration::from_millis(measure_ms)))
        );
    }

    let _ = writeln!(output, "== clocksource");
    let clocksource = current_clocksource();
    let available = sys::read_line(kernel::AVAILABLE_CLOCKSOURCES).unwrap_or_default();
    let mut names = Vec::new();
    for token in available
        .split(|&byte| byte == b' ')
        .filter(|token| !token.is_empty())
    {
        add_unique(&mut names, String::from_utf8_lossy(token));
    }
    let available = joined(&names, "unknown");
    let _ = writeln!(
        output,
        "current: {}\navailable: {available}",
        String::from_utf8_lossy(&clocksource)
    );

    let _ = writeln!(output, "== cpuinfo");
    cpuinfo_summary(output);
    let (present, absent): (Vec<&str>, Vec<&str>) = TSC_FLAGS
        .iter()
        .copied()
        .partition(|flag| cpuinfo_has_flag(flag.as_bytes()));
    let (present, absent) = (joined(&present, "none"), joined(&absent, "none"));
    let _ = writeln!(
        output,
        "time flags present: {present}\ntime flags absent: {absent}"
    );
    let _ = writeln!(output, "== cpuid");
    let ids = print_cpuid(output);
    if read_msrs {
        let _ = writeln!(output, "== msr (cpu 0)");
        print_msrs(output);
    }

    let _ = writeln!(output, "== kernel");
    if let Ok(line) = sys::read_line(kernel::COMMAND_LINE) {
        let _ = write!(output, "cmdline: ");
        let _ = output.write_all(&line);
        let _ = writeln!(output);
    }
    if let Ok(line) = sys::read_line(kernel::ONLINE_CPUS) {
        let _ = write!(output, "online cpus: ");
        let _ = output.write_all(&line);
        let _ = writeln!(output);
    }
    for path in PARAM_FILES {
        let name = path.to_string_lossy();
        match sys::read_line(path) {
            Ok(line) => {
                let _ = write!(output, "{name} = ");
                let _ = output.write_all(&line);
                let _ = writeln!(output);
            }
            Err(_) => {
                let _ = writeln!(output, "{name}: absent");
            }
        }
    }

    let _ = writeln!(output, "== kmsg: hypervisor");
    let mut hypervisor = Filter::default();
    match print_kmsg(output, HYPERVISOR_PATTERNS, &mut hypervisor) {
        Err(error) => {
            let _ = writeln!(output, "(kernel log unavailable: {})", error.desc());
        }
        Ok(()) if hypervisor.matches == 0 => {
            let _ = writeln!(output, "(none)");
        }
        Ok(()) => {}
    }
    let _ = writeln!(output, "== kmsg: time");
    let mut time = Filter::default();
    match print_kmsg(output, TIME_PATTERNS, &mut time) {
        Err(error) => {
            let _ = writeln!(output, "(kernel log unavailable: {})", error.desc());
        }
        Ok(()) if time.matches == 0 => {
            let _ = writeln!(output, "(none)");
        }
        Ok(()) => {}
    }
    let _ = writeln!(output, "== timer_list");
    let timer_summary = print_timer_list(output);
    let _ = writeln!(
        output,
        "NVX-TIME-PROBE clock clocksource={} available={available} tsc_hz={} tsc_hz_source={} \
         {} \
         flags={present} missing_flags={absent} tsc_unstable_msgs={} unchecked_msr_msgs={} {}",
        String::from_utf8_lossy(&clocksource),
        f0(rate.hz),
        rate.source,
        ids.hypervisor_marker_fields(),
        time.tsc_unstable,
        time.unchecked_msr,
        timer_summary
    );
    Ok(())
}

#[cfg(test)]
mod tests;
