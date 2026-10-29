//! Conformance checks C1 through C11 and the CI-only exhaustive check.

use std::ffi::CStr;
use std::io::Write;
use std::thread;
use std::time::{Duration, Instant};

use super::Phase;
use super::cpu_mask::CpuMask;
use super::decoders::{self, CpuInfo, ZERO_HYPERV_LEAVES};
use super::events::{CheckId, DETAIL_MAX, ViolationCode, escape_text};
use super::state::{CheckState, CheckStatus, TimeState};
use super::system::{
    DAEMON_NAME, DAEMON_PID_PATH, DEFAULT_STALL_TIMEOUT_S, MsrDescriptor, Runtime, msr_error,
    rates_load, state_apply, state_load,
};
use crate::guest::status::CommandError;
use crate::guest::sys::x86::cpuid::{self, Register};
use crate::guest::sys::x86::{CpuidResult, msr};
use crate::guest::sys::{self, Errno, MsrAccess, MsrFile, ProcessId, kernel};

/// Maximum timer-list convergence wait during boot and restore.
pub(crate) const TIMER_LIST_WAIT: Duration = Duration::from_millis(200);
/// Poll interval while the timer list converges.
const TIMER_LIST_POLL: Duration = Duration::from_millis(10);
/// Maximum wait for persisted phase checks to complete.
const STATUS_WAIT: Duration = Duration::from_secs(30);
/// Poll interval while waiting for persisted checks.
const STATUS_POLL: Duration = Duration::from_millis(10);
/// Hz in a kHz, which `/proc/cpuinfo`'s rates use.
const HZ_PER_KHZ: u64 = 1000;
/// The process ID of init, which never runs the daemon.
const INIT_PROCESS_ID: i64 = 1;

/// The hypervisor vendor signature of Hyper-V's CPUID leaf 0x40000000, "Microsoft Hv".
const HYPERV_VENDOR: [u32; 3] = [
    u32::from_le_bytes(*b"Micr"),
    u32::from_le_bytes(*b"osof"),
    u32::from_le_bytes(*b"t Hv"),
];
/// Last Hyper-V identity leaf that OpenVMM implements, which leaf 0x40000000 returns.
const HYPERV_MAXIMUM_LEAF: u32 = cpuid::LEAF_HYPERVISOR + 5;
/// The interface signature of Hyper-V's CPUID leaf 0x40000001, "Hv#1".
const HYPERV_INTERFACE: u32 = u32::from_le_bytes(*b"Hv#1");
/// The build of leaf 0x40000002, OpenVMM's NVX build: "NVX", then the time ABI version as the
/// major version in the upper half of EBX, version 2.0.
const HYPERV_BUILD: [u32; 2] = [
    u32::from_le_bytes(*b"NVX\0"),
    (crate::contract::TIME_ABI_VERSION as u32) << 16,
];
/// The partition privileges of leaf 0x40000003's EAX: the hypercall, VP-index, frequency, and
/// TSC invariant-control MSRs.
const HYPERV_PRIVILEGES: u32 = (1 << cpuid::HYPERV_EAX_HYPERCALL)
    | (1 << cpuid::HYPERV_EAX_VP_INDEX)
    | (1 << cpuid::HYPERV_EAX_ACCESS_FREQUENCY)
    | (1 << cpuid::HYPERV_EAX_ACCESS_TSC_INVARIANT);
/// The features of leaf 0x40000003's EDX: the frequency MSRs are available.
const HYPERV_FEATURES: u32 = 1 << cpuid::HYPERV_EDX_FREQUENCY_MSRS;
/// The spinlock-retry recommendation of leaf 0x40000004's EBX: never notify the hypervisor.
const HYPERV_NEVER_NOTIFY: u32 = u32::MAX;

/// The LAPIC timer rates that OpenVMM declares in the APIC-frequency MSR: 1 GHz or 200 MHz.
const LAPIC_RATES_HZ: [u64; 2] = [1_000_000_000, 200_000_000];
/// The value of the TSC invariant-control MSR that exposes the invariant TSC.
const TSC_INVARIANT_ENABLED: u64 = 1;
/// The invariant-control value that the MSR rejects, the first above its one valid bit.
const TSC_INVARIANT_REJECTED: u64 = 2;
/// The Hyper-V MSRs whose reads must fault, because OpenVMM does not implement them for NVX.
const FAULTING_READS: [u32; 3] = [
    msr::HV_GUEST_OS_ID,
    msr::HV_HYPERCALL,
    msr::HV_TIME_REF_COUNT,
];
/// The MSRs whose writes must fault in the exhaustive check.
const FAULTING_WRITES: [u32; 6] = [
    msr::HV_GUEST_OS_ID,
    msr::HV_HYPERCALL,
    msr::HV_VP_INDEX,
    msr::HV_TIME_REF_COUNT,
    msr::HV_TSC_FREQUENCY,
    msr::HV_APIC_FREQUENCY,
];
/// The architectural MSRs whose reads must fault in the exhaustive check.
const FAULTING_ARCHITECTURAL_READS: [u32; 2] = [msr::IA32_TSC_ADJUST, msr::IA32_TSC_DEADLINE];
/// The hypervisor leaves that the exhaustive check scans one by one, after the identity leaves.
const EXHAUSTIVE_LEAVES: (u32, u32, usize) = (0x4000_0006, 0x4000_00ff, 1);
/// The bases of the hypervisor ranges that the exhaustive check scans, such as KVM's.
const EXHAUSTIVE_BASES: (u32, u32, usize) = (0x4000_0100, 0x4000_ff00, 0x100);
/// The IDs of the exhaustive check's subchecks.
const EXHAUSTIVE_CHECKS: [&str; 6] = ["X1", "X2", "X3", "X4", "X5", "X6"];
/// The IDs of the exhaustive subchecks that read MSRs.
const EXHAUSTIVE_MSR_CHECKS: [&str; 4] = ["X3", "X4", "X5", "X6"];

/// One CPUID feature bit that the time ABI requires set or clear.
struct FeatureBit {
    /// The leaf that holds the bit.
    leaf: u32,
    /// The subleaf, for a leaf that has subleaves.
    subleaf: Option<u32>,
    /// The register of the CPUID result that holds the bit.
    register: Register,
    /// The bit's number.
    bit: u32,
    /// The feature's name, which the diagnostic gives.
    name: &'static str,
    /// Whether the bit must be set.
    set: bool,
}

impl FeatureBit {
    /// Returns the bit's description, such as `CPUID.1:ECX[31] (hypervisor)`.
    fn label(&self) -> String {
        let subleaf = self
            .subleaf
            .map_or_else(String::new, |subleaf| format!(".{subleaf}"));
        format!(
            "CPUID.{:X}{subleaf}:{}[{}] ({})",
            self.leaf,
            self.register.name(),
            self.bit,
            self.name
        )
    }
}

/// The time bits of leaf 1 that the time ABI requires.
const LEAF1_TIME_BITS: [FeatureBit; 4] = [
    FeatureBit {
        leaf: cpuid::LEAF_FEATURES,
        subleaf: None,
        register: Register::Ecx,
        bit: cpuid::FEATURES_ECX_HYPERVISOR,
        name: "hypervisor",
        set: true,
    },
    FeatureBit {
        leaf: cpuid::LEAF_FEATURES,
        subleaf: None,
        register: Register::Ecx,
        bit: cpuid::FEATURES_ECX_TSC_DEADLINE,
        name: "TSC-deadline",
        set: false,
    },
    FeatureBit {
        leaf: cpuid::LEAF_FEATURES,
        subleaf: None,
        register: Register::Ecx,
        bit: cpuid::FEATURES_ECX_PDCM,
        name: "PDCM",
        set: false,
    },
    FeatureBit {
        leaf: cpuid::LEAF_FEATURES,
        subleaf: None,
        register: Register::Edx,
        bit: cpuid::FEATURES_EDX_TSC,
        name: "TSC",
        set: true,
    },
];
/// Leaf 7's `IA32_TSC_ADJUST` bit, which the time ABI requires clear.
const TSC_ADJUST_BIT: FeatureBit = FeatureBit {
    leaf: cpuid::LEAF_STRUCTURED_FEATURES,
    subleaf: Some(0),
    register: Register::Ebx,
    bit: cpuid::STRUCTURED_FEATURES_EBX_TSC_ADJUST,
    name: "TSC_ADJUST",
    set: false,
};
/// Leaf 0x80000001's `RDTSCP` bit, which the time ABI requires set.
const RDTSCP_BIT: FeatureBit = FeatureBit {
    leaf: cpuid::LEAF_EXTENDED_FEATURES,
    subleaf: None,
    register: Register::Edx,
    bit: cpuid::EXTENDED_FEATURES_EDX_RDTSCP,
    name: "RDTSCP",
    set: true,
};
/// Basic leaves whose every register the time ABI requires zero, when the processor has them:
/// performance monitoring, the TSC crystal ratio, and the frequencies.
const ZERO_BASIC_LEAVES: [u32; 3] = [
    cpuid::LEAF_PERFORMANCE_MONITORING,
    cpuid::LEAF_TSC_CRYSTAL,
    cpuid::LEAF_FREQUENCY,
];

/// Formats a CPUID result as the checks print it.
fn registers(value: CpuidResult) -> String {
    cpuid::hex_registers(value, ":")
}

/// Returns an MSR access's value, or zero, and its negative `errno`, or zero, as the
/// diagnostics print them.
fn value_and_error(result: &Result<u64, Errno>) -> (u64, i32) {
    (*result.as_ref().unwrap_or(&0), msr_error(result))
}

/// One conformance failure: the check that failed, and its diagnostic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Failure {
    /// The check that failed.
    id: CheckId,
    /// The diagnostic, which the violation event escapes byte for byte.
    detail: Vec<u8>,
}

impl Failure {
    /// Returns the failure of check `id`, with `detail`.
    fn new(id: CheckId, detail: impl Into<Vec<u8>>) -> Self {
        Self {
            id,
            detail: detail.into(),
        }
    }
}

/// The outcome of one conformance check, which reports its first failure.
pub(crate) type Verdict = Result<(), Failure>;

#[derive(Clone)]
/// Mutable evidence and failure count for one conformance-check phase.
pub(crate) struct Checks {
    /// Phase whose failures are being reported.
    pub(crate) phase: Phase,
    /// Number of failed checks.
    pub(crate) failures: usize,
    /// Number of possible CPUs.
    pub(crate) possible: usize,
    /// The online CPUs.
    pub(crate) online: CpuMask,
    /// Parsed per-CPU frequency evidence.
    pub(crate) cpuinfo: CpuInfo,
    /// Validated TSC frequency.
    pub(crate) tsc_hz: u64,
    /// Validated LAPIC timer frequency.
    pub(crate) lapic_hz: u64,
}

impl Checks {
    /// Creates an empty check set for `phase`.
    pub(crate) fn empty(phase: Phase) -> Self {
        Self {
            phase,
            failures: 0,
            possible: 0,
            online: CpuMask::EMPTY,
            cpuinfo: CpuInfo::default(),
            tsc_hz: 0,
            lapic_hz: 0,
        }
    }

    /// Loads the possible and online CPUs, which must each be a nonempty CPU list.
    pub(crate) fn load(phase: Phase) -> Result<Self, Errno> {
        let cpus = |path| {
            let list = sys::read_line(path)?;
            CpuMask::parse(&list)
                .filter(|cpus| !cpus.is_empty())
                .ok_or(Errno::EINVAL)
        };
        Ok(Self {
            possible: cpus(kernel::POSSIBLE_CPUS)?.len(),
            online: cpus(kernel::ONLINE_CPUS)?,
            ..Self::empty(phase)
        })
    }

    /// Records one failure under an explicit ABI code.
    pub(crate) fn fail_code(
        &mut self,
        runtime: &Runtime<'_>,
        code: ViolationCode,
        detail: impl AsRef<[u8]>,
    ) -> bool {
        self.failures += 1;
        runtime.conformance_failure(code, self.phase, detail.as_ref());
        false
    }

    /// Records one `G_CONFORMANCE_Cn` failure.
    pub(crate) fn fail(
        &mut self,
        runtime: &Runtime<'_>,
        id: CheckId,
        detail: impl AsRef<[u8]>,
    ) -> bool {
        self.fail_code(runtime, ViolationCode::Conformance(id), detail)
    }

    /// Records the failure of a check that failed, and returns whether it passed.
    pub(crate) fn record(&mut self, runtime: &Runtime<'_>, verdict: Verdict) -> bool {
        match verdict {
            Ok(()) => true,
            Err(Failure { id, detail }) => self.fail(runtime, id, detail),
        }
    }
}

/// Publishes one completed phase's outcome, timing, failures, and rates. Report-only mode records
/// the failures, and fails a phase that has any.
pub(crate) fn record_check(
    runtime: &Runtime<'_>,
    checks: &Checks,
    elapsed_us: i64,
    cpu_us: i64,
    tsc_hz: u64,
    lapic_hz: u64,
) -> Result<(), Errno> {
    let phase = checks.phase;
    if phase.check_index().is_none() {
        return Err(Errno::EINVAL);
    }
    let failures = runtime.report_only().then_some(checks.failures as u64);
    let status = if failures.is_some_and(|failures| failures > 0) {
        CheckStatus::Fail
    } else {
        CheckStatus::Ok
    };
    state_apply(|state| {
        let generation = if phase == Phase::Boot {
            0
        } else {
            state.generation
        };
        if let Some(check) = state.check_mut(phase) {
            *check = CheckState {
                status: Some(status),
                cpus: checks.online.len() as u64,
                elapsed_us,
                cpu_us,
                generation,
                failures,
            };
        }
        if tsc_hz != 0 {
            state.tsc_hz = tsc_hz;
        }
        if lapic_hz != 0 {
            state.lapic_hz = lapic_hz;
        }
    })
}

/// Validates the Hyper-V CPUID identity leaves on one CPU, of `possible` possible CPUs.
fn check_identity(possible: usize, cpu: usize) -> Verdict {
    let capacity = possible as u32;
    let expected = [
        CpuidResult {
            eax: HYPERV_MAXIMUM_LEAF,
            ebx: HYPERV_VENDOR[0],
            ecx: HYPERV_VENDOR[1],
            edx: HYPERV_VENDOR[2],
        },
        CpuidResult {
            eax: HYPERV_INTERFACE,
            ..cpuid::ZERO
        },
        CpuidResult {
            eax: HYPERV_BUILD[0],
            ebx: HYPERV_BUILD[1],
            ..cpuid::ZERO
        },
        CpuidResult {
            eax: HYPERV_PRIVILEGES,
            edx: HYPERV_FEATURES,
            ..cpuid::ZERO
        },
        CpuidResult {
            ebx: HYPERV_NEVER_NOTIFY,
            ..cpuid::ZERO
        },
        CpuidResult {
            eax: capacity,
            ebx: capacity,
            ..cpuid::ZERO
        },
    ];
    for (leaf, expected) in (cpuid::LEAF_HYPERVISOR..).zip(expected) {
        let value = sys::x86::cpuid(leaf, 0);
        if value != expected {
            return Err(Failure::new(
                CheckId::C1,
                format!(
                    "cpu {cpu} leaf 0x{leaf:08x} is {}, expected {}",
                    registers(value),
                    registers(expected)
                ),
            ));
        }
    }
    for leaf in ZERO_HYPERV_LEAVES.into_iter().flatten() {
        let value = sys::x86::cpuid(leaf, 0);
        if value != cpuid::ZERO {
            return Err(Failure::new(
                CheckId::C1,
                format!(
                    "cpu {cpu} leaf 0x{leaf:08x} is {}, expected zero",
                    registers(value)
                ),
            ));
        }
    }
    Ok(())
}

/// Validates one CPUID feature bit in `value`, the result of its leaf.
fn check_bit(cpu: usize, bit: &FeatureBit, value: CpuidResult) -> Verdict {
    if cpuid::bit(bit.register.of(value), bit.bit) == bit.set {
        return Ok(());
    }
    Err(Failure::new(
        CheckId::C2,
        format!(
            "cpu {cpu} {} is {}",
            bit.label(),
            if bit.set { "clear" } else { "set" }
        ),
    ))
}

/// Validates one complete CPUID leaf value.
fn check_leaf(cpu: usize, leaf: u32, expected: CpuidResult) -> Verdict {
    let value = sys::x86::cpuid(leaf, 0);
    if value == expected {
        return Ok(());
    }
    Err(Failure::new(
        CheckId::C2,
        format!(
            "cpu {cpu} leaf 0x{leaf:08x} is {}, expected {}",
            registers(value),
            registers(expected)
        ),
    ))
}

/// Validates time-related architectural and Hyper-V feature bits.
fn check_time_bits(cpu: usize) -> Verdict {
    let maximum = sys::x86::cpuid(cpuid::LEAF_MAXIMUM, 0).eax;
    let features = sys::x86::cpuid(cpuid::LEAF_FEATURES, 0);
    for bit in &LEAF1_TIME_BITS {
        check_bit(cpu, bit, features)?;
    }
    if maximum < cpuid::LEAF_POWER_MANAGEMENT {
        return Err(Failure::new(
            CheckId::C2,
            format!(
                "cpu {cpu} lacks leaf {:#x} (ARAT)",
                cpuid::LEAF_POWER_MANAGEMENT
            ),
        ));
    }
    let arat = CpuidResult {
        eax: 1 << cpuid::POWER_MANAGEMENT_EAX_ARAT,
        ..cpuid::ZERO
    };
    check_leaf(cpu, cpuid::LEAF_POWER_MANAGEMENT, arat)?;
    if maximum >= cpuid::LEAF_STRUCTURED_FEATURES {
        let structured = sys::x86::cpuid(cpuid::LEAF_STRUCTURED_FEATURES, 0);
        check_bit(cpu, &TSC_ADJUST_BIT, structured)?;
    }
    for leaf in ZERO_BASIC_LEAVES {
        if maximum >= leaf {
            check_leaf(cpu, leaf, cpuid::ZERO)?;
        }
    }
    if sys::x86::cpuid(cpuid::LEAF_EXTENDED_MAXIMUM, 0).eax < cpuid::LEAF_ADVANCED_POWER {
        return Err(Failure::new(
            CheckId::C2,
            format!("cpu {cpu} lacks leaf {:#x}", cpuid::LEAF_ADVANCED_POWER),
        ));
    }
    let extended = sys::x86::cpuid(cpuid::LEAF_EXTENDED_FEATURES, 0);
    check_bit(cpu, &RDTSCP_BIT, extended)?;
    let invariant_tsc = CpuidResult {
        edx: 1 << cpuid::ADVANCED_POWER_EDX_INVARIANT_TSC,
        ..cpuid::ZERO
    };
    check_leaf(cpu, cpuid::LEAF_ADVANCED_POWER, invariant_tsc)
}

/// Validates one CPU's VP index MSR, CPU zero's TSC rate MSR, and, with `full`, CPU zero's
/// LAPIC rate, invariant-control, and faulting MSRs. Records the validated rates in `checks`.
fn check_msrs(runtime: &Runtime<'_>, checks: &mut Checks, cpu: usize, full: bool) -> Verdict {
    let failure = |detail: String| Failure::new(CheckId::C3, format!("cpu {cpu}: {detail}"));
    let descriptor = runtime
        .ops
        .open_msr(cpu, MsrAccess::Read)
        .map_err(|error| failure(format!("open {}: {}", MsrFile::path(cpu), error.desc())))?;
    let read = |register| runtime.ops.read_msr(&descriptor, register);
    if full {
        let index = read(msr::HV_VP_INDEX);
        if index != Ok(cpu as u64) {
            let (value, error) = value_and_error(&index);
            return Err(failure(format!(
                "MSR {:#010x} (VP index) is {value} (error {error})",
                msr::HV_VP_INDEX
            )));
        }
    }
    if cpu != 0 {
        return Ok(());
    }
    let tsc = read(msr::HV_TSC_FREQUENCY).map_err(|error| {
        failure(format!(
            "MSR {:#010x} read: {}",
            msr::HV_TSC_FREQUENCY,
            error.desc()
        ))
    })?;
    if checks.tsc_hz != 0 && tsc != checks.tsc_hz {
        return Err(failure(format!(
            "MSR {:#010x} is {tsc} Hz, expected {} Hz",
            msr::HV_TSC_FREQUENCY,
            checks.tsc_hz
        )));
    }
    checks.tsc_hz = tsc;
    if !full {
        return Ok(());
    }
    let lapic = read(msr::HV_APIC_FREQUENCY);
    match lapic {
        Ok(rate)
            if LAPIC_RATES_HZ.contains(&rate)
                && (checks.lapic_hz == 0 || rate == checks.lapic_hz) =>
        {
            checks.lapic_hz = rate;
        }
        _ => {
            let (value, error) = value_and_error(&lapic);
            return Err(failure(format!(
                "MSR {:#010x} (LAPIC rate) is {value} (error {error})",
                msr::HV_APIC_FREQUENCY
            )));
        }
    }
    let control = read(msr::HV_TSC_INVARIANT_CONTROL);
    if control != Ok(TSC_INVARIANT_ENABLED) {
        let (value, error) = value_and_error(&control);
        return Err(failure(format!(
            "MSR {:#010x} is {value} (error {error})",
            msr::HV_TSC_INVARIANT_CONTROL
        )));
    }
    for register in FAULTING_READS {
        let result = read(register);
        if result != Err(Errno::EIO) {
            return Err(failure(format!(
                "MSR {register:#010x} read did not fail with EIO (error {})",
                msr_error(&result)
            )));
        }
    }
    Ok(())
}

/// Reads the whole of the kernel file at `path`, or fails check `id` with a diagnostic that
/// names the file, such as `read /proc/cmdline: No such file or directory`.
fn read_kernel_file(id: CheckId, path: &CStr) -> Result<Vec<u8>, Failure> {
    sys::read_file(path).map_err(|error| {
        Failure::new(
            id,
            format!("read {}: {}", path.to_string_lossy(), error.desc()),
        )
    })
}

/// Validates `/proc/cpuinfo` for the selected CPUs, and records the CPUs' rates in `checks`.
fn check_cpuinfo(checks: &mut Checks, selected: CpuMask, flags: bool) -> Verdict {
    let text = read_kernel_file(CheckId::C5, kernel::CPU_INFORMATION)?;
    let (info, verdict) = decoders::check_cpuinfo(&text, selected, flags);
    checks.cpuinfo = info;
    verdict.map_err(|detail| Failure::new(CheckId::C5, detail))
}

/// Validates current and available clocksources from sysfs.
fn check_clocksource() -> Verdict {
    let read = |path| {
        sys::read_file(path).map_err(|error| {
            Failure::new(CheckId::C6, format!("clocksource sysfs: {}", error.desc()))
        })
    };
    let current = read(kernel::CURRENT_CLOCKSOURCE)?;
    let available = read(kernel::AVAILABLE_CLOCKSOURCES)?;
    decoders::check_clocksource(&current, &available)
        .map_err(|detail| Failure::new(CheckId::C6, detail))
}

/// Validates `/proc/timer_list` for the online CPUs, waiting up to `wait` for it to converge.
pub(crate) fn check_timer_list(online: CpuMask, wait: Duration) -> Verdict {
    let deadline = Instant::now() + wait;
    loop {
        let text = read_kernel_file(CheckId::C7, kernel::TIMER_LIST)?;
        match decoders::timer_list_verdict(&text, online) {
            Ok(()) => return Ok(()),
            Err(detail) if Instant::now() >= deadline => {
                return Err(Failure::new(CheckId::C7, detail));
            }
            Err(_) => thread::sleep(TIMER_LIST_POLL),
        }
    }
}

/// Validates absence of unsupported platform devices and RCU settings.
fn check_platform() -> Verdict {
    if sys::path_exists(kernel::VMBUS) {
        return Err(Failure::new(
            CheckId::C8,
            format!("{} exists", kernel::VMBUS.to_string_lossy()),
        ));
    }
    if sys::path_exists(kernel::CPU_FREQUENCY_POLICY) {
        return Err(Failure::new(CheckId::C8, "cpufreq policy0 exists"));
    }
    // An unreadable setting prints as -1, as in the C program.
    let suppress = sys::read_number::<i64>(kernel::RCU_STALL_SUPPRESS).unwrap_or(-1);
    if suppress != 0 {
        return Err(Failure::new(
            CheckId::C8,
            format!("rcu_cpu_stall_suppress is {suppress}"),
        ));
    }
    let timeout = sys::read_number::<i64>(kernel::RCU_STALL_TIMEOUT).unwrap_or(-1);
    if timeout != DEFAULT_STALL_TIMEOUT_S {
        return Err(Failure::new(
            CheckId::C8,
            format!("rcu_cpu_stall_timeout is {timeout}"),
        ));
    }
    Ok(())
}

/// Validates the guest kernel command line.
fn check_cmdline() -> Verdict {
    let text = read_kernel_file(CheckId::C9, kernel::COMMAND_LINE)?;
    decoders::check_cmdline(&text).map_err(|detail| Failure::new(CheckId::C9, detail))
}

/// Finds and validates the published `nvx-time` daemon process.
pub(crate) fn daemon_pid() -> Option<ProcessId> {
    let pid = sys::read_number::<i64>(DAEMON_PID_PATH).ok()?;
    let process = ProcessId::from_positive(pid).filter(|_| pid > INIT_PROCESS_ID)?;
    if !sys::process_exists(process) {
        return None;
    }
    is_daemon_name(&sys::process_name(process).ok()?).then_some(process)
}

/// Returns whether `comm`, the contents of a procfs `comm` file, is the daemon's name.
fn is_daemon_name(comm: &[u8]) -> bool {
    comm.strip_suffix(b"\n") == Some(DAEMON_NAME.to_bytes())
}

/// Validates daemon liveness, state, violations, and RCU stalls.
fn check_daemon() -> Verdict {
    if daemon_pid().is_none() {
        return Err(Failure::new(CheckId::C10, "the time daemon is not running"));
    }
    let Ok(state) = state_load() else {
        return Err(Failure::new(CheckId::C10, "time state is unreadable"));
    };
    if state.violations != 0 {
        return Err(Failure::new(
            CheckId::C10,
            format!("the time daemon recorded {} violations", state.violations),
        ));
    }
    let stalls = sys::read_number::<i64>(kernel::RCU_STALL_COUNT).unwrap_or(-1);
    if stalls != 0 {
        return Err(Failure::new(
            CheckId::C10,
            format!("rcu_stall_count is {stalls}"),
        ));
    }
    Ok(())
}

/// Validates the soft-lockup and hung-task detectors when the kernel has either.
fn check_debug_watchdogs() -> Verdict {
    // A missing or unreadable setting prints as -1, as in the C program.
    let setting =
        |path| sys::path_exists(path).then(|| sys::read_number::<i64>(path).unwrap_or(-1));
    let (soft, hung) = (
        setting(kernel::SOFT_WATCHDOG),
        setting(kernel::HUNG_TASK_TIMEOUT),
    );
    if soft.is_none() && hung.is_none() {
        return Ok(());
    }
    if soft != Some(1) {
        return Err(Failure::new(
            CheckId::C11,
            format!("soft_watchdog is {}", soft.unwrap_or(-1)),
        ));
    }
    match hung {
        Some(timeout) if timeout != 0 => Ok(()),
        _ => Err(Failure::new(
            CheckId::C11,
            format!("hung_task_timeout_secs is {}", hung.unwrap_or(-1)),
        )),
    }
}

/// Runs selected identity and MSR checks on one pinned CPU.
fn check_one_cpu(
    runtime: &Runtime<'_>,
    base: &Checks,
    cpu: usize,
    identity: bool,
    msrs: bool,
) -> Checks {
    let mut checks = base.clone();
    checks.failures = 0;
    if let Err(error) = sys::pin_current_thread(cpu) {
        checks.fail(
            runtime,
            CheckId::C1,
            format!("cannot run on cpu {cpu}: {}", error.desc()),
        );
        return checks;
    }
    if identity {
        checks.record(runtime, check_identity(checks.possible, cpu));
        checks.record(runtime, check_time_bits(cpu));
    }
    if msrs {
        let verdict = check_msrs(runtime, &mut checks, cpu, true);
        checks.record(runtime, verdict);
    }
    checks
}

/// Runs selected per-CPU checks on this thread, which returns to its original affinity, until a
/// CPU fails.
fn check_cpus(
    runtime: &Runtime<'_>,
    checks: &mut Checks,
    selected: CpuMask,
    identity: bool,
    msrs: bool,
) {
    let _affinity = match sys::AffinityGuard::save() {
        Ok(affinity) => affinity,
        Err(error) => {
            checks.fail(
                runtime,
                CheckId::C1,
                format!("sched_getaffinity: {}", error.desc()),
            );
            return;
        }
    };
    for cpu in selected.iter() {
        let child = check_one_cpu(runtime, checks, cpu, identity, msrs);
        checks.failures += child.failures;
        if cpu == 0 && msrs {
            checks.tsc_hz = child.tsc_hz;
            checks.lapic_hz = child.lapic_hz;
        }
        if child.failures != 0 {
            return;
        }
    }
}

/// The kernel log that the boot checks read from a buffer allocated before they start.
pub(crate) struct BootLog<'a> {
    /// The log's size, as `klogctl` returned it, which a diagnostic gives.
    pub(crate) size: i64,
    /// The buffer, unless it could not be allocated.
    pub(crate) buffer: Option<&'a mut [u8]>,
}

/// Validates boot-log evidence from a preallocated kernel-log buffer.
fn boot_log(runtime: &Runtime<'_>, checks: &mut Checks, log: Option<BootLog<'_>>) {
    let Some(BootLog { size, buffer }) = log else {
        return;
    };
    let Some(buffer) = buffer else {
        checks.fail(
            runtime,
            CheckId::C4,
            format!("no buffer for the {size}-byte kernel log"),
        );
        return;
    };
    let count = match sys::kernel::read_kernel_log(buffer) {
        Ok(count) => count,
        Err(error) => {
            checks.fail(
                runtime,
                CheckId::C4,
                format!("read the kernel log: {}", error.desc()),
            );
            return;
        }
    };
    let log = decoders::scan_boot_log(&buffer[..count], true);
    if let Err(detail) = log.verdict() {
        checks.fail(runtime, CheckId::C4, detail);
    }
    if let Some(wx) = &log.wx {
        checks.fail_code(runtime, ViolationCode::KernelWx, wx);
    }
}

/// Runs a side check on a copy of `base` that starts without failures, and returns the copy.
fn side_check(base: &Checks, check: impl FnOnce(&mut Checks)) -> Checks {
    let mut side = base.clone();
    side.failures = 0;
    check(&mut side);
    side
}

/// Runs every boot check except C12 in the same per-CPU and side-job parallel shape as C. With
/// one online CPU, every check runs on this thread, as they do when the thread may hold port
/// permission, which the threads would inherit.
pub(crate) fn run_boot_checks(
    runtime: &Runtime<'_>,
    checks: &mut Checks,
    log: Option<BootLog<'_>>,
) {
    let base = checks.clone();
    let selected = checks.online;
    let timer_list = |side: &mut Checks| {
        side.record(runtime, check_timer_list(selected, TIMER_LIST_WAIT));
    };
    let cpuinfo = |side: &mut Checks| {
        let verdict = check_cpuinfo(side, selected, true);
        side.record(runtime, verdict);
    };
    if selected.len() <= 1 || sys::check_released().is_err() {
        run_boot_foreground(runtime, checks);
        checks.failures += side_check(&base, |side| boot_log(runtime, side, log)).failures;
        checks.failures += side_check(&base, timer_list).failures;
        let cpuinfo = side_check(&base, cpuinfo);
        checks.failures += cpuinfo.failures;
        checks.cpuinfo = cpuinfo.cpuinfo;
        check_cpus(runtime, checks, selected, true, true);
        compare_cpu_rate(runtime, checks);
        return;
    }
    thread::scope(|scope| {
        let log = scope.spawn(|| side_check(&base, |side| boot_log(runtime, side, log)));
        let timer = scope.spawn(|| side_check(&base, timer_list));
        let cpuinfo = scope.spawn(|| side_check(&base, cpuinfo));
        let cpus = selected
            .iter()
            .map(|cpu| {
                let base = &base;
                (
                    cpu,
                    scope.spawn(move || check_one_cpu(runtime, base, cpu, true, true)),
                )
            })
            .collect::<Vec<_>>();

        run_boot_foreground(runtime, checks);

        for side in [log.join(), timer.join()] {
            match side {
                Ok(side) => checks.failures += side.failures,
                Err(_) => {
                    checks.fail(runtime, CheckId::C10, "a boot side job crashed");
                }
            }
        }
        match cpuinfo.join() {
            Ok(side) => {
                checks.failures += side.failures;
                checks.cpuinfo = side.cpuinfo;
            }
            Err(_) => {
                checks.fail(runtime, CheckId::C10, "the cpuinfo side job crashed");
            }
        }
        for (cpu, handle) in cpus {
            match handle.join() {
                Ok(child) => {
                    checks.failures += child.failures;
                    if cpu == 0 {
                        checks.tsc_hz = child.tsc_hz;
                        checks.lapic_hz = child.lapic_hz;
                    }
                }
                Err(_) => {
                    checks.fail(runtime, CheckId::C1, format!("the cpu {cpu} job crashed"));
                }
            }
        }
    });
    compare_cpu_rate(runtime, checks);
}

/// Runs the boot checks that run on the caller's thread: C9, C8, C6, and C11.
fn run_boot_foreground(runtime: &Runtime<'_>, checks: &mut Checks) {
    let foreground: [fn() -> Verdict; 4] = [
        check_cmdline,
        check_platform,
        check_clocksource,
        check_debug_watchdogs,
    ];
    for check in foreground {
        checks.record(runtime, check());
    }
}

/// Cross-checks the validated TSC rate against CPU zero's cpuinfo rate.
fn compare_cpu_rate(runtime: &Runtime<'_>, checks: &mut Checks) {
    if checks.tsc_hz != 0
        && let Some(khz) = checks.cpuinfo.khz[0]
        && checks.tsc_hz / HZ_PER_KHZ != khz
    {
        checks.fail(
            runtime,
            CheckId::C3,
            format!(
                "cpu 0: floor(F / 1000) is {} kHz but cpu MHz is {khz} kHz",
                checks.tsc_hz / HZ_PER_KHZ,
            ),
        );
    }
}

/// Runs checks that must precede snapshot capture.
pub(crate) fn run_capture_checks(runtime: &Runtime<'_>, checks: &mut Checks) {
    if let Ok((tsc, lapic)) = rates_load() {
        checks.tsc_hz = tsc;
        checks.lapic_hz = lapic;
    }
    // Each failure counts a violation in the state before C10 reads the count.
    checks.record(runtime, check_clocksource());
    checks.record(runtime, check_timer_list(checks.online, Duration::ZERO));
    checks.record(runtime, check_daemon());
}

/// Runs restore checks, including checks on newly online CPUs.
pub(crate) fn run_restore_checks(runtime: &Runtime<'_>, checks: &mut Checks, new_cpus: CpuMask) {
    match rates_load() {
        Ok((tsc, lapic)) => {
            checks.tsc_hz = tsc;
            checks.lapic_hz = lapic;
        }
        Err(_) => {
            checks.fail(runtime, CheckId::C3, "the boot rates are unavailable");
        }
    }
    if !new_cpus.is_empty() {
        check_cpus(runtime, checks, new_cpus, true, false);
        let verdict = check_cpuinfo(checks, new_cpus, true);
        checks.record(runtime, verdict);
    }
    let verdict = check_msrs(runtime, checks, 0, false);
    checks.record(runtime, verdict);
    checks.record(runtime, check_clocksource());
    checks.record(runtime, check_daemon());
}

/// Waits for boot-only or all persisted checks and returns completion state.
pub(crate) fn wait_for_checks(boot_only: bool) -> (TimeState, bool) {
    let deadline = Instant::now() + STATUS_WAIT;
    loop {
        let state = state_load().unwrap_or_default();
        if !state.pending(boot_only) {
            return (state, true);
        }
        if Instant::now() >= deadline {
            return (state, false);
        }
        thread::sleep(STATUS_POLL);
    }
}

/// Aggregate counters and rates for the exhaustive CI-only report.
struct Exhaustive {
    /// Number of failed exhaustive subchecks.
    failures: usize,
    /// First validated TSC rate.
    tsc_hz: u64,
    /// First validated LAPIC rate.
    lapic_hz: u64,
    /// Parsed CPU frequency evidence.
    cpuinfo: CpuInfo,
}

/// The outcome of one exhaustive subcheck: the diagnostic of its pass, or of its failure.
type Outcome = Result<String, String>;

/// Emits one stable exhaustive-check report line and counts a failure.
fn exhaustive_report(
    output: &mut dyn Write,
    state: &mut Exhaustive,
    id: &str,
    cpu: usize,
    outcome: &Outcome,
) {
    let (status, detail) = match outcome {
        Ok(detail) => ("pass", detail),
        Err(detail) => ("fail", detail),
    };
    let _ = writeln!(
        output,
        "NVX-TIME-ABI-EXHAUSTIVE: v=1 check={id} cpu={cpu} status={status} detail=\"{}\"",
        escape_text(detail.as_bytes(), DETAIL_MAX - 1),
    );
    if outcome.is_err() {
        state.failures += 1;
    }
}

/// Scans a sparse CPUID leaf range, `(first, last, step)`, for X1 or X2, and summarizes it.
fn exhaustive_leaves((first, last, step): (u32, u32, usize), out_of_range: CpuidResult) -> Outcome {
    let mut count = 0;
    let mut failing = 0;
    let mut first_failure = None;
    for leaf in (first..=last).step_by(step) {
        count += 1;
        if let Err(detail) =
            decoders::exhaustive_leaf_ok(leaf, sys::x86::cpuid(leaf, 0), out_of_range)
        {
            failing += 1;
            first_failure.get_or_insert(detail);
        }
    }
    match first_failure {
        None => Ok(format!(
            "{count} {} read zero or the highest basic leaf",
            if step == 1 { "leaves" } else { "bases" }
        )),
        Some(detail) if failing == 1 => Err(detail),
        Some(detail) => Err(format!("{detail}; {failing} of {count} fail")),
    }
}

/// Validates one CPU's VP index, rate, invariant-control, and faulting MSRs for X3, against
/// the first CPU's rates and cpuinfo, and returns the CPU's TSC and LAPIC rates.
fn exhaustive_rates(
    runtime: &Runtime<'_>,
    state: &Exhaustive,
    descriptor: &MsrDescriptor,
    cpu: usize,
) -> Result<(u64, u64), String> {
    let read = |register| runtime.ops.read_msr(descriptor, register);
    let index = read(msr::HV_VP_INDEX);
    if index != Ok(cpu as u64) {
        let (index, error) = value_and_error(&index);
        return Err(format!(
            "MSR {:#010x} (VP index) is {index} (error {error})",
            msr::HV_VP_INDEX
        ));
    }
    let (tsc, error) = value_and_error(&read(msr::HV_TSC_FREQUENCY));
    if error != 0 || tsc == 0 || (state.tsc_hz != 0 && tsc != state.tsc_hz) {
        return Err(format!(
            "MSR {:#010x} (TSC rate) is {tsc} (error {error}; first CPU {})",
            msr::HV_TSC_FREQUENCY,
            state.tsc_hz
        ));
    }
    if let Some(khz) = state.cpuinfo.khz[cpu]
        && tsc / HZ_PER_KHZ != khz
    {
        return Err(format!(
            "floor(F / 1000) is {} kHz but cpu MHz is {khz} kHz",
            tsc / HZ_PER_KHZ
        ));
    }
    let (lapic, error) = value_and_error(&read(msr::HV_APIC_FREQUENCY));
    if error != 0
        || !LAPIC_RATES_HZ.contains(&lapic)
        || (state.lapic_hz != 0 && lapic != state.lapic_hz)
    {
        return Err(format!(
            "MSR {:#010x} (LAPIC rate) is {lapic} (error {error}; first CPU {})",
            msr::HV_APIC_FREQUENCY,
            state.lapic_hz
        ));
    }
    let control = read(msr::HV_TSC_INVARIANT_CONTROL);
    if control != Ok(TSC_INVARIANT_ENABLED) {
        let (control, error) = value_and_error(&control);
        return Err(format!(
            "MSR {:#010x} is {control} (error {error})",
            msr::HV_TSC_INVARIANT_CONTROL
        ));
    }
    for register in FAULTING_READS {
        let result = read(register);
        if result != Err(Errno::EIO) {
            return Err(format!(
                "MSR {register:#010x} read did not fail with EIO (error {})",
                msr_error(&result)
            ));
        }
    }
    Ok((tsc, lapic))
}

/// Writes 0 and 1 to the invariant-control MSR, reading each back, then the rejected 2.
fn probe_invariant_control(
    runtime: &Runtime<'_>,
    descriptor: &MsrDescriptor,
) -> Result<(), String> {
    let control = msr::HV_TSC_INVARIANT_CONTROL;
    for written in 0..=TSC_INVARIANT_ENABLED {
        let write_error = msr_error(&runtime.ops.write_msr(descriptor, control, written));
        let (value, read_error) = value_and_error(&runtime.ops.read_msr(descriptor, control));
        let error = if write_error != 0 {
            write_error
        } else {
            read_error
        };
        if error != 0 || value != written {
            return Err(format!(
                "after writing {written}, MSR {control:#010x} is {value} (error {error})"
            ));
        }
    }
    let rejected = runtime
        .ops
        .write_msr(descriptor, control, TSC_INVARIANT_REJECTED);
    if rejected != Err(Errno::EIO) {
        return Err(format!(
            "write of {TSC_INVARIANT_REJECTED} to MSR {control:#010x} did not fail with EIO \
             (error {})",
            msr_error(&rejected)
        ));
    }
    Ok(())
}

/// Validates for X4 that the invariant-control MSR takes 0 and 1 and rejects 2, and leaves it
/// at 1, which exposes the invariant TSC, whatever it found.
fn exhaustive_invariant_control(runtime: &Runtime<'_>, descriptor: &MsrDescriptor) -> Outcome {
    let control = msr::HV_TSC_INVARIANT_CONTROL;
    let probed = probe_invariant_control(runtime, descriptor);
    let _ = runtime
        .ops
        .write_msr(descriptor, control, TSC_INVARIANT_ENABLED);
    probed?;
    let (value, error) = value_and_error(&runtime.ops.read_msr(descriptor, control));
    if error != 0 || value != TSC_INVARIANT_ENABLED {
        return Err(format!("MSR {control:#010x} is {value} after the check"));
    }
    Ok("0 and 1 accepted, 2 rejected, left at 1".to_owned())
}

/// Validates that writes of zero to the Hyper-V MSRs fault with `EIO`, for X5, or that reads of
/// the architectural MSRs do, for X6.
fn exhaustive_faults(runtime: &Runtime<'_>, descriptor: &MsrDescriptor, writes: bool) -> Outcome {
    let (registers, access, accesses): (&[u32], _, _) = if writes {
        (&FAULTING_WRITES, "write of 0 to", "writes")
    } else {
        (&FAULTING_ARCHITECTURAL_READS, "read of", "reads")
    };
    for &register in registers {
        let error = if writes {
            msr_error(&runtime.ops.write_msr(descriptor, register, 0))
        } else {
            msr_error(&runtime.ops.read_msr(descriptor, register))
        };
        if error != -(Errno::EIO as i32) {
            return Err(format!(
                "{access} MSR {register:#x} did not fail with EIO (error {error})"
            ));
        }
    }
    Ok(format!("{} {accesses} fail with EIO", registers.len()))
}

/// Runs every CI-only exhaustive check on every online CPU, from this thread, which returns to
/// its original affinity.
pub(crate) fn exhaustive(
    runtime: &Runtime<'_>,
    output: &mut dyn Write,
) -> Result<(), CommandError> {
    let mut state = Exhaustive {
        failures: 0,
        tsc_hz: 0,
        lapic_hz: 0,
        cpuinfo: CpuInfo::default(),
    };
    let online = sys::read_line(kernel::ONLINE_CPUS)
        .ok()
        .and_then(|list| CpuMask::parse(&list));
    let Some(cpus) = online.filter(|cpus| !cpus.is_empty()) else {
        let _ = writeln!(
            output,
            "NVX-TIME-ABI-EXHAUSTIVE: v=1 status=fail cpus=0 failures=1"
        );
        return Err(CommandError::Operation);
    };
    if let Ok(text) = sys::read_file(kernel::CPU_INFORMATION) {
        state.cpuinfo = decoders::check_cpuinfo(&text, cpus, false).0;
    }
    let _affinity = sys::AffinityGuard::save().ok();
    for cpu in cpus.iter() {
        if sys::pin_current_thread(cpu).is_err() {
            let outcome = Err("cannot run on this CPU".to_owned());
            for id in EXHAUSTIVE_CHECKS {
                exhaustive_report(output, &mut state, id, cpu, &outcome);
            }
            continue;
        }
        let basic = sys::x86::cpuid(cpuid::LEAF_MAXIMUM, 0);
        let out_of_range = sys::x86::cpuid(basic.eax, 0);
        for (id, leaves) in [("X1", EXHAUSTIVE_LEAVES), ("X2", EXHAUSTIVE_BASES)] {
            let outcome = exhaustive_leaves(leaves, out_of_range);
            exhaustive_report(output, &mut state, id, cpu, &outcome);
        }
        let descriptor = match runtime.ops.open_msr(cpu, MsrAccess::ReadWrite) {
            Ok(descriptor) => descriptor,
            Err(error) => {
                let outcome = Err(format!("open {}: {}", MsrFile::path(cpu), error.desc()));
                for id in EXHAUSTIVE_MSR_CHECKS {
                    exhaustive_report(output, &mut state, id, cpu, &outcome);
                }
                continue;
            }
        };
        let rates = exhaustive_rates(runtime, &state, &descriptor, cpu).map(|(tsc, lapic)| {
            if state.tsc_hz == 0 {
                state.tsc_hz = tsc;
                state.lapic_hz = lapic;
            }
            format!("vp_index={cpu} tsc_hz={tsc} lapic_hz={lapic} invariant_control=1")
        });
        exhaustive_report(output, &mut state, "X3", cpu, &rates);
        let control = exhaustive_invariant_control(runtime, &descriptor);
        exhaustive_report(output, &mut state, "X4", cpu, &control);
        for (id, writes) in [("X5", true), ("X6", false)] {
            let faults = exhaustive_faults(runtime, &descriptor, writes);
            exhaustive_report(output, &mut state, id, cpu, &faults);
        }
    }
    let status = if state.failures == 0 { "ok" } else { "fail" };
    let _ = writeln!(
        output,
        "NVX-TIME-ABI-EXHAUSTIVE: v=1 status={status} cpus={} failures={}",
        cpus.len(),
        state.failures
    );
    if state.failures == 0 {
        Ok(())
    } else {
        Err(CommandError::Operation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_daemon_is_recognized_by_the_name_that_it_takes() {
        let comm = thread::spawn(|| {
            sys::set_command_name(DAEMON_NAME).expect("set the thread's name");
            std::fs::read("/proc/thread-self/comm").expect("read the thread's name")
        })
        .join()
        .expect("the naming thread panicked");
        assert!(is_daemon_name(&comm));
        assert!(!is_daemon_name(b"nvx-init\n"));
        assert!(!is_daemon_name(b"nvx-time"));
        assert!(!is_daemon_name(b"nvx-time-promot\n"));
    }

    #[test]
    fn kernel_files_are_read_whole_or_fail_their_check_naming_the_file() {
        assert_eq!(
            read_kernel_file(CheckId::C9, kernel::COMMAND_LINE),
            Ok(std::fs::read("/proc/cmdline").expect("read the kernel command line"))
        );
        assert_eq!(
            read_kernel_file(CheckId::C7, c"/proc/nvx-time-missing"),
            Err(Failure::new(
                CheckId::C7,
                "read /proc/nvx-time-missing: No such file or directory"
            ))
        );
    }

    #[test]
    fn wait_timeout_math_uses_the_abi_constants() {
        assert_eq!(STATUS_WAIT, Duration::from_nanos(30_000_000_000));
        assert_eq!(STATUS_POLL, Duration::from_nanos(10_000_000));
        assert_eq!(TIMER_LIST_WAIT, Duration::from_nanos(200_000_000));
    }

    #[test]
    fn identity_constants_match_the_hyperv_leaves() {
        assert_eq!(HYPERV_MAXIMUM_LEAF, 0x4000_0005);
        assert_eq!(HYPERV_VENDOR, [0x7263_694d, 0x666f_736f, 0x7648_2074]);
        assert_eq!(HYPERV_INTERFACE, 0x3123_7648);
        assert_eq!(HYPERV_BUILD, [0x0058_564e, 0x0002_0000]);
        assert_eq!(HYPERV_PRIVILEGES, 0x8860);
        assert_eq!(HYPERV_FEATURES, 0x100);
    }

    #[test]
    fn feature_bits_are_labeled_as_the_c_program_labels_them() {
        let labels = LEAF1_TIME_BITS
            .iter()
            .chain([&TSC_ADJUST_BIT, &RDTSCP_BIT])
            .map(FeatureBit::label)
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            [
                "CPUID.1:ECX[31] (hypervisor)",
                "CPUID.1:ECX[24] (TSC-deadline)",
                "CPUID.1:ECX[15] (PDCM)",
                "CPUID.1:EDX[4] (TSC)",
                "CPUID.7.0:EBX[1] (TSC_ADJUST)",
                "CPUID.80000001:EDX[27] (RDTSCP)",
            ]
        );
    }

    #[test]
    fn msr_diagnostics_name_the_registers_in_hexadecimal() {
        assert_eq!(format!("{:#010x}", msr::HV_VP_INDEX), "0x40000002");
        assert_eq!(format!("{:#x}", msr::IA32_TSC_DEADLINE), "0x6e0");
        assert_eq!(MsrFile::path(3), "/dev/cpu/3/msr");
    }
}
