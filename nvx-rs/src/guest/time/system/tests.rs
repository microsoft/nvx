//! A fake of the privileged operations, and checks of the runtime over it.

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use super::*;
use crate::guest::sys::tests::PortBackend;
use crate::guest::time::NSEC_PER_MSEC;
use crate::guest::time::events::CheckId;
use crate::guest::time::state::HostClockStatus;

/// Privileged operations that record their requests and replay canned port input.
pub(crate) struct FakeOps {
    /// The error with which the kernel refuses to grant the port permission, if any.
    pub(crate) grant_error: Option<Errno>,
    /// The error with which the kernel refuses to revoke the port permission, if any.
    pub(crate) revoke_error: Option<Errno>,
    /// Changes of the port permission, as `(first, count, enable)`, in order.
    pub(crate) permissions: Mutex<Vec<(u16, u16, bool)>>,
    /// Values that successive port reads return.
    pub(crate) input: Mutex<VecDeque<u32>>,
    /// Port writes, in order.
    pub(crate) output: Mutex<Vec<(Port, u8)>>,
    /// Clock adjustments, in order.
    pub(crate) adjustments: Mutex<Vec<ClockAdjustment>>,
    /// The last power-off status, or zero.
    pub(crate) power: AtomicUsize,
    /// Kernel log records, in order.
    pub(crate) kernel_log: Mutex<Vec<u8>>,
    /// Console output, in order.
    pub(crate) console: Mutex<Vec<u8>>,
    /// What the frequency MSR reads: the declared TSC rate, or an error.
    pub(crate) tsc_hz: Result<u64, Errno>,
    /// The next TSC read, which each read advances by [`FakeOps::TSC_STEP`].
    pub(crate) tsc: AtomicU64,
    /// What every read of the realtime clock returns.
    pub(crate) realtime_ns: i64,
}

impl FakeOps {
    /// The fake's declared TSC rate: 2 GHz, two cycles a nanosecond.
    pub(crate) const TSC_HZ: u64 = 2_000_000_000;
    /// The fake's first TSC read.
    pub(crate) const TSC_START: u64 = 1_000_000;
    /// The cycles by which each TSC read advances the next.
    pub(crate) const TSC_STEP: u64 = 100;
    /// The fake's realtime clock.
    pub(crate) const REALTIME_NS: i64 = 1_790_841_600_000_000_000;

    /// Returns a fake whose port reads return `input`.
    pub(crate) fn new(input: impl IntoIterator<Item = u32>) -> Self {
        Self {
            grant_error: None,
            revoke_error: None,
            permissions: Mutex::new(Vec::new()),
            input: Mutex::new(input.into_iter().collect()),
            output: Mutex::new(Vec::new()),
            adjustments: Mutex::new(Vec::new()),
            power: AtomicUsize::new(0),
            kernel_log: Mutex::new(Vec::new()),
            console: Mutex::new(Vec::new()),
            tsc_hz: Ok(Self::TSC_HZ),
            tsc: AtomicU64::new(Self::TSC_START),
            realtime_ns: Self::REALTIME_NS,
        }
    }

    /// Returns the number of transactions that took the port permission, each of which
    /// grants two ranges.
    pub(crate) fn transactions(&self) -> usize {
        let permissions = self.permissions.lock().unwrap();
        permissions.iter().filter(|(.., enable)| *enable).count() / 2
    }
}

/// Returns the port input of a time sample of `generation` whose guest TSC is `tsc` and whose
/// host UTC is `utc_ns`, paired within `pairing_ns`, and which publishes `host`: the window's
/// four-byte reads.
pub(crate) fn sample_input(
    generation: u32,
    utc_ns: u64,
    tsc: u64,
    pairing_ns: u32,
    host: HostClock,
) -> Vec<u32> {
    let unknown = |bound: Option<u32>| bound.unwrap_or(u32::MAX);
    let mut sample = [0_u8; SAMPLE_SIZE];
    sample[..4].copy_from_slice(&[1, 0, host_status_byte(host.status), 0]);
    sample[4..8].copy_from_slice(&generation.to_le_bytes());
    sample[8..16].copy_from_slice(&utc_ns.to_le_bytes());
    sample[16..24].copy_from_slice(&tsc.to_le_bytes());
    sample[24..28].copy_from_slice(&pairing_ns.to_le_bytes());
    sample[28..32].copy_from_slice(&host.disruption.to_le_bytes());
    sample[32..40].copy_from_slice(&host.correction.to_le_bytes());
    sample[40..44].copy_from_slice(&unknown(host.correction_bound).to_le_bytes());
    sample[44..48].copy_from_slice(&unknown(host.max_error_us).to_le_bytes());
    sample
        .chunks(size_of::<u32>())
        .map(|word| u32::from_ne_bytes(word.try_into().unwrap()))
        .collect()
}

/// Returns the byte that encodes `status` in a time sample.
fn host_status_byte(status: HostClockStatus) -> u8 {
    match status {
        HostClockStatus::Unknown => 0,
        HostClockStatus::Synchronized => 1,
        HostClockStatus::FreeRunning => 2,
        HostClockStatus::Unreliable => 3,
    }
}

/// Returns a guest TSC midway between the fake's reads around the selector write of the
/// `index`th time sample that it latches, counting from zero: each sample reads the TSC three
/// times.
pub(crate) fn sample_tsc(index: u64) -> u64 {
    FakeOps::TSC_START + 3 * index * FakeOps::TSC_STEP + FakeOps::TSC_STEP / 2
}

impl PortBackend for FakeOps {
    fn set_permission(&self, first: u16, count: u16, enable: bool) -> Result<(), Errno> {
        self.permissions
            .lock()
            .unwrap()
            .push((first, count, enable));
        let error = if enable {
            self.grant_error
        } else {
            self.revoke_error
        };
        error.map_or(Ok(()), Err)
    }

    fn read_u8(&self, _: Port) -> u8 {
        self.input.lock().unwrap().pop_front().unwrap_or_default() as u8
    }

    fn read_u32(&self, _: Port) -> u32 {
        self.input.lock().unwrap().pop_front().unwrap_or_default()
    }

    fn write_u8(&self, port: Port, value: u8) {
        self.output.lock().unwrap().push((port, value));
    }
}

impl PrivilegedOps for FakeOps {
    fn read_tsc(&self) -> u64 {
        self.tsc.fetch_add(Self::TSC_STEP, Ordering::Relaxed)
    }

    fn realtime_ns(&self) -> i64 {
        self.realtime_ns
    }

    fn open_ports(&self) -> Result<PortAccess<'_>, PortError> {
        PortAccess::with_backend(self)
    }

    fn open_msr(&self, _: usize, _: MsrAccess) -> Result<MsrDescriptor, Errno> {
        Ok(MsrDescriptor::Fake)
    }

    fn read_msr(&self, _: &MsrDescriptor, register: u32) -> Result<u64, Errno> {
        match register {
            msr::HV_TSC_FREQUENCY => self.tsc_hz,
            _ => Err(Errno::EIO),
        }
    }

    fn write_msr(&self, _: &MsrDescriptor, _: u32, _: u64) -> Result<(), Errno> {
        Err(Errno::EIO)
    }

    fn adjust_clock(&self, adjustment: ClockAdjustment) -> Result<ClockDiscipline, Errno> {
        self.adjustments.lock().unwrap().push(adjustment);
        Ok(ClockDiscipline::default())
    }

    fn power_off(&self, status: PowerOffStatus) {
        self.power
            .store(usize::from(status.code()), Ordering::Relaxed);
    }

    fn write_kernel_log(&self, record: &[u8]) {
        self.kernel_log.lock().unwrap().extend_from_slice(record);
    }

    fn write_console(&self, separator: &[u8], line: &[u8]) {
        let mut console = self.console.lock().unwrap();
        console.extend_from_slice(separator);
        console.extend_from_slice(line);
    }
}

#[test]
fn port_reader_uses_one_four_byte_read_per_chunk() {
    let ops = FakeOps::new([0x0403_0201, 0x0807_0605]);
    let ports = ops.open_ports().unwrap();
    let mut bytes = [0; 7];
    portb_read(&ports, Port::PortbData, &mut bytes);
    ports.release().unwrap();
    assert_eq!(bytes, [1, 2, 3, 4, 5, 6, 7]);
    assert!(ops.input.lock().unwrap().is_empty());
}

#[test]
fn packet_without_ranges_costs_exactly_twenty_six_data_reads() {
    let ops = FakeOps::new(0..26);
    let ports = ops.open_ports().unwrap();
    let mut header = [0; decoders::PACKET_HEADER_SIZE];
    let mut entropy = [0; decoders::PACKET_ENTROPY_SIZE];
    let mut dns_server = [0; decoders::PACKET_DNS_SIZE];
    portb_read(&ports, Port::PortbData, &mut header);
    portb_read(&ports, Port::PortbData, &mut entropy);
    portb_read(&ports, Port::PortbData, &mut dns_server);
    ports.release().unwrap();
    assert!(ops.input.lock().unwrap().is_empty());
    // A time sample costs twelve.
    let ops = FakeOps::new(0..12);
    let ports = ops.open_ports().unwrap();
    let mut sample = [0; SAMPLE_SIZE];
    portb_read(&ports, Port::PortbWindow, &mut sample);
    ports.release().unwrap();
    assert!(ops.input.lock().unwrap().is_empty());
}

#[test]
fn msr_errors_print_as_negative_errno_values() {
    let ops = FakeOps::new([]);
    let runtime = Runtime::new(&ops, false);
    let descriptor = MsrDescriptor::Fake;
    let read = runtime.ops.read_msr(&descriptor, msr::HV_GUEST_OS_ID);
    assert_eq!(msr_error(&read), -(Errno::EIO as i32));
    let written = runtime.ops.write_msr(&descriptor, msr::HV_GUEST_OS_ID, 0);
    assert_eq!(msr_error(&written), -(Errno::EIO as i32));
    assert_eq!(msr_error(&Ok::<u64, Errno>(1)), 0);
}

#[test]
fn a_refused_port_permission_reaches_the_sample_diagnostic() {
    let mut ops = FakeOps::new([]);
    ops.grant_error = Some(Errno::EPERM);
    let runtime = Runtime::new(&ops, false);
    let error = best_sample(&runtime, 3, NSEC_PER_MSEC, 0).unwrap_err();
    assert_eq!(error, SampleError::Ports(PortError::Grant(Errno::EPERM)));
    assert_eq!(
        error.to_string(),
        format!("ioperm: {}", Errno::EPERM.desc())
    );
    assert!(ops.output.lock().unwrap().is_empty());
    assert_eq!(sys::check_released(), Ok(()));
}

#[test]
fn a_sample_is_one_transaction_that_ends_before_it_returns() {
    let ops = FakeOps::new([0]);
    let runtime = Runtime::new(&ops, false);
    let error = best_sample(&runtime, 3, NSEC_PER_MSEC, 0).unwrap_err();
    assert_eq!(error, SampleError::NoTimeWindow(0));
    assert_eq!(
        *ops.permissions.lock().unwrap(),
        [
            (0xe9, 6, true),
            (0x604, 2, true),
            (0xe9, 6, false),
            (0x604, 2, false)
        ]
    );
    assert_eq!(sys::check_released(), Ok(()));
}

#[test]
fn a_failed_revocation_keeps_the_time_tools_from_starting_a_process() {
    let mut ops = FakeOps::new([u32::from(PORTB_TIME_WINDOW)]);
    ops.revoke_error = Some(Errno::ENOMEM);
    let runtime = Runtime::new(&ops, false);
    let error = best_sample(&runtime, 0, NSEC_PER_MSEC, 0).unwrap_err();
    assert_eq!(error, SampleError::Ports(PortError::Revoke(Errno::ENOMEM)));
    assert_eq!(
        error.to_string(),
        format!("ioperm revoke: {}", Errno::ENOMEM.desc())
    );
    // `fork_process` refuses before it forks: `fork` itself refuses with `EBUSY` in a
    // process with more than one thread, such as the test harness.
    let refused = fork_process().unwrap_err();
    assert_eq!(refused, SpawnError::Ports(PortError::Revoke(Errno::ENOMEM)));
    assert_eq!(refused.to_string(), error.to_string());
}

#[test]
fn each_event_writes_the_debug_console_in_a_transaction_of_its_own() {
    let ops = FakeOps::new([]);
    let runtime = Runtime::new(&ops, true);
    runtime.repair_failure(ViolationCode::RepairSuppression, b"one");
    runtime.repair_failure(ViolationCode::RepairSuppression, b"two");
    assert_eq!(ops.transactions(), 2);
    assert_eq!(sys::check_released(), Ok(()));
    let console = ops.console.lock().unwrap().clone();
    let port: Vec<_> = console
        .iter()
        .map(|&byte| (Port::PortbData, byte))
        .collect();
    assert_eq!(*ops.output.lock().unwrap(), port);
}

#[test]
fn an_event_skips_the_debug_console_without_the_port_permission() {
    let mut ops = FakeOps::new([]);
    ops.grant_error = Some(Errno::EPERM);
    let runtime = Runtime::new(&ops, true);
    runtime.repair_failure(ViolationCode::RepairSuppression, b"cannot restore");
    assert!(!ops.console.lock().unwrap().is_empty());
    assert!(!ops.kernel_log.lock().unwrap().is_empty());
    assert!(ops.output.lock().unwrap().is_empty());
    assert_eq!(sys::check_released(), Ok(()));
}

#[test]
fn an_event_logs_a_failed_revocation_after_its_debug_console_write() {
    let mut ops = FakeOps::new([]);
    ops.revoke_error = Some(Errno::ENOMEM);
    let runtime = Runtime::new(&ops, true);
    runtime.repair_failure(ViolationCode::RepairSuppression, b"cannot restore");
    let line = ops.console.lock().unwrap().clone();
    let port: Vec<_> = line.iter().map(|&byte| (Port::PortbData, byte)).collect();
    assert_eq!(*ops.output.lock().unwrap(), port);
    let diagnostic = format!("nvx-time: ioperm revoke: {}\n", Errno::ENOMEM.desc());
    assert_eq!(
        *ops.kernel_log.lock().unwrap(),
        [
            KERNEL_LOG_PRIORITY,
            &line,
            DIAGNOSTIC_PRIORITY,
            diagnostic.as_bytes()
        ]
        .concat()
    );
    assert_eq!(sys::check_released(), Err(PortError::Revoke(Errno::ENOMEM)));
}

/// The host clock of the fake's samples: synchronized, 100 ppm fast, to 1 ppb, within 2 ms.
const HOST: HostClock = HostClock {
    status: HostClockStatus::Synchronized,
    disruption: 1,
    correction: 6_553_600,
    correction_bound: Some(66),
    max_error_us: Some(2_000),
};

/// Returns the port input of port B's status, with the time window, then of `samples`.
fn window_input(samples: impl IntoIterator<Item = Vec<u32>>) -> Vec<u32> {
    std::iter::once(u32::from(PORTB_TIME_WINDOW))
        .chain(samples.into_iter().flatten())
        .collect()
}

#[test]
fn a_sample_from_another_generation_is_a_generation_error() {
    let ops = FakeOps::new(window_input([sample_input(7, 1, sample_tsc(0), 0, HOST)]));
    let runtime = Runtime::new(&ops, false);
    let error = best_sample(&runtime, 1, NSEC_PER_MSEC, 6).unwrap_err();
    assert_eq!(
        error,
        SampleError::Generation {
            sample: 7,
            expected: 6
        }
    );
    assert_eq!(error.to_string(), "time sample generation 7, expected 6");
    assert_eq!(
        ops.output.lock().unwrap().as_slice(),
        [(Port::PortbStatus, SELECT_TIME_SAMPLE)]
    );
}

#[test]
fn a_sample_pairs_its_guest_tsc_with_the_realtime_clock() {
    // The sample's TSC is midway between the reads around the selector write, 100 cycles,
    // 50 ns, before the realtime read, whose own reads are 100 cycles apart.
    let utc_ns = FakeOps::REALTIME_NS as u64 + 1_000;
    let ops = FakeOps::new(window_input([sample_input(
        0,
        utc_ns,
        sample_tsc(0),
        400,
        HOST,
    )]));
    let runtime = Runtime::new(&ops, false);
    let (result, sample) = best_sample(&runtime, 3, SAMPLE_BOUND_NS, 0).unwrap();
    assert_eq!(result, SampleResult::Accepted);
    assert_eq!(
        sample,
        BestSample {
            theta: 1_050,
            epsilon: 400 + 1_000 + 25 + 1,
            host: HOST,
            frequency: 6_553_600,
            limited: false,
        }
    );
    // One transaction latched one sample, after the rate's MSR read and a query of the
    // frequency.
    assert_eq!(ops.transactions(), 1);
    assert_eq!(
        ops.output.lock().unwrap().as_slice(),
        [(Port::PortbStatus, SELECT_TIME_SAMPLE)]
    );
    assert_eq!(
        ops.adjustments.lock().unwrap().as_slice(),
        [ClockAdjustment::Query]
    );
}

#[test]
fn a_sample_outside_the_guest_tsc_reads_is_rejected() {
    // The tolerance is the pairing and the skew bound, 1.4 µs: 2,800 cycles.
    let ops = FakeOps::new(window_input([sample_input(
        0,
        1,
        FakeOps::TSC_START - 2_801,
        400,
        HOST,
    )]));
    let runtime = Runtime::new(&ops, false);
    let error = best_sample(&runtime, 1, SAMPLE_BOUND_NS, 0).unwrap_err();
    assert_eq!(
        error,
        SampleError::Invalid(Some(TimeSampleError::Tsc {
            tsc: FakeOps::TSC_START - 2_801,
            before: FakeOps::TSC_START,
            after: FakeOps::TSC_START + FakeOps::TSC_STEP,
        }))
    );
    assert_eq!(error.rejection(), SampleRejection::Tsc);
    assert_eq!(
        SampleError::Invalid(Some(TimeSampleError::Version(1))).rejection(),
        SampleRejection::Invalid
    );
    assert_eq!(
        SampleError::NoTimeWindow(0).rejection(),
        SampleRejection::Invalid
    );
}

#[test]
fn a_wide_sample_gives_way_to_a_narrow_one() {
    let utc_ns = FakeOps::REALTIME_NS as u64;
    let wide = sample_input(0, utc_ns, sample_tsc(0), 150_000, HOST);
    let ops = FakeOps::new(window_input([wide.clone()]));
    let runtime = Runtime::new(&ops, false);
    let (result, sample) = best_sample(&runtime, 1, SAMPLE_BOUND_NS, 0).unwrap();
    assert_eq!((result, sample.epsilon), (SampleResult::Wide, 151_026));
    let narrow = sample_input(0, utc_ns, sample_tsc(1), 600, HOST);
    let ops = FakeOps::new(window_input([wide, narrow]));
    let runtime = Runtime::new(&ops, false);
    let (result, sample) = best_sample(&runtime, 3, SAMPLE_BOUND_NS, 0).unwrap();
    assert_eq!((result, sample.epsilon), (SampleResult::Accepted, 1_626));
    assert_eq!(ops.output.lock().unwrap().len(), 2);
}

#[test]
fn a_sample_needs_the_declared_tsc_rate() {
    for (rate, message) in [
        (
            Err(Errno::EACCES),
            format!("MSR 0x40000022 read: {}", Errno::EACCES.desc()),
        ),
        (
            Ok(999_999),
            "MSR 0x40000022 (TSC rate) is 999999 Hz".to_owned(),
        ),
    ] {
        let mut ops = FakeOps::new(window_input([]));
        ops.tsc_hz = rate;
        let runtime = Runtime::new(&ops, false);
        let error = best_sample(&runtime, 1, SAMPLE_BOUND_NS, 0).unwrap_err();
        assert_eq!(error, SampleError::TscRate(rate));
        assert_eq!(error.to_string(), message);
        // The sample took no port transaction.
        assert!(ops.permissions.lock().unwrap().is_empty());
    }
    // The rate that the boot checks recorded needs no MSR.
    let mut ops = FakeOps::new([]);
    ops.tsc_hz = Err(Errno::EIO);
    let runtime = Runtime::new(&ops, false);
    runtime.set_tsc_hz(0);
    assert!(runtime.tsc_hz().is_err());
    runtime.set_tsc_hz(3_000_000_000);
    assert_eq!(runtime.tsc_hz(), Ok(3_000_000_000));
    // The MSR's rate is read once.
    let ops = FakeOps::new([]);
    let runtime = Runtime::new(&ops, false);
    assert_eq!(runtime.tsc_hz(), Ok(FakeOps::TSC_HZ));
    assert_eq!(runtime.tsc_hz.load(Ordering::Relaxed), FakeOps::TSC_HZ);
}

#[test]
fn privileged_adjustment_and_power_paths_are_injectable() {
    let ops = FakeOps::new([]);
    let runtime = Runtime::new(&ops, false);
    step_clock(&runtime, -1_128_000_000).unwrap();
    let hold = FrequencyHold {
        scaled_ppm: 65_536,
        maximum_error_us: 3,
        estimated_error_us: 1,
    };
    hold_frequency(&runtime, hold).unwrap();
    assert_eq!(current_frequency(&runtime), 0);
    assert_eq!(
        ops.adjustments.lock().unwrap().as_slice(),
        [
            ClockAdjustment::Step {
                seconds: -2,
                nanoseconds: 872_000_000,
            },
            ClockAdjustment::Frequency(hold),
            ClockAdjustment::Query,
        ]
    );
    runtime.ops.power_off(PowerOffStatus::Repair);
    assert_eq!(ops.power.load(Ordering::Relaxed), 195);
}

#[test]
fn a_synchronization_steps_then_holds_the_host_frequency() {
    let ops = FakeOps::new([]);
    let runtime = Runtime::new(&ops, false);
    let sample = BestSample {
        theta: -1_128_000_000,
        epsilon: 1_500,
        host: HOST,
        frequency: 6_553_600,
        limited: false,
    };
    synchronize(&runtime, &sample).unwrap();
    assert_eq!(
        ops.adjustments.lock().unwrap().as_slice(),
        [
            ClockAdjustment::Step {
                seconds: -2,
                nanoseconds: 872_000_000,
            },
            // The maximum error adds the synchronized host's 2 ms.
            ClockAdjustment::Frequency(FrequencyHold {
                scaled_ppm: 6_553_600,
                maximum_error_us: 2 + 2_000,
                estimated_error_us: 2,
            }),
        ]
    );
    assert_eq!(
        String::from_utf8(wide_sample(100_001)).unwrap(),
        "time sample uncertainty 100001 ns exceeds 100000 ns"
    );
}

#[test]
fn repair_failures_report_restore_events_and_power_off_only_when_enforcing() {
    for (report_only, strict, prefix, power) in [
        (false, false, "NVX-TIME-ABI", PowerOffStatus::Repair.code()),
        (false, true, "NVX-TIME-ABI", 0),
        (true, false, "NVX-TIME-REPORT", 0),
        (true, true, "NVX-TIME-REPORT", 0),
    ] {
        let ops = FakeOps::new([]);
        let runtime = Runtime::new(&ops, report_only);
        let runtime = if strict { runtime.strict() } else { runtime };
        assert_eq!(runtime.is_strict(), strict && !report_only);
        runtime.set_generation(4);
        runtime.repair_failure(ViolationCode::RepairSuppression, b"cannot restore");
        let line = String::from_utf8(ops.console.lock().unwrap().clone()).unwrap();
        let start = format!(
            "{prefix}-VIOLATION: v=1 code=G_REPAIR_SUPPRESSION source=repair phase=restore \
                 generation=4 boottime_ns="
        );
        assert!(line.starts_with(&start), "{line}");
        assert!(line.ends_with(" detail=\"cannot restore\"\n"), "{line}");
        assert_eq!(
            *ops.kernel_log.lock().unwrap(),
            [KERNEL_LOG_PRIORITY, line.as_bytes()].concat()
        );
        let port: Vec<_> = line.bytes().map(|byte| (Port::PortbData, byte)).collect();
        assert_eq!(*ops.output.lock().unwrap(), port);
        assert_eq!(ops.power.load(Ordering::Relaxed), usize::from(power));
        // A strict runtime keeps the violation instead, with enforcing mode's status.
        let kept = runtime.take_violation();
        if runtime.is_strict() {
            let violation = kept.unwrap();
            assert_eq!(
                (violation.status(), violation.code(), violation.detail()),
                (195, "G_REPAIR_SUPPRESSION", "cannot restore")
            );
        } else {
            assert_eq!(kept, None);
        }
    }
}

#[test]
fn a_strict_runtime_keeps_its_first_violation_and_never_powers_off() {
    let ops = FakeOps::new([]);
    let runtime = Runtime::new(&ops, false).strict();
    runtime.conformance_failure(
        ViolationCode::Conformance(CheckId::C12),
        Phase::Boot,
        b"first",
    );
    runtime.repair_failure(ViolationCode::RepairSuppression, b"second");
    runtime.fail_fatal(
        PowerOffStatus::Violation,
        ViolationCode::RcuStall,
        EventSource::Watcher,
        Phase::Runtime,
        b"third",
    );
    assert_eq!(ops.power.load(Ordering::Relaxed), 0);
    let console = String::from_utf8(ops.console.lock().unwrap().clone()).unwrap();
    assert_eq!(
        console.matches("NVX-TIME-ABI-VIOLATION: ").count(),
        3,
        "{console}"
    );
    let violation = runtime.take_violation().unwrap();
    assert_eq!(
        (violation.status(), violation.code(), violation.detail()),
        (193, "G_CONFORMANCE_C12", "first")
    );
    assert_eq!(runtime.take_violation(), None);
    // The statuses are those that the protocol publishes for the host.
    assert_eq!(
        [
            PowerOffStatus::Conformance,
            PowerOffStatus::Violation,
            PowerOffStatus::Repair
        ]
        .map(PowerOffStatus::code),
        [
            status::TIME_CONFORMANCE,
            status::TIME_VIOLATION,
            status::TIME_REPAIR
        ]
    );
}

#[test]
fn modes_of_the_runtime_files_match_the_c_program() {
    assert_eq!(DIRECTORY_MODE.bits(), 0o755);
    assert_eq!(PUBLIC_FILE_MODE.bits(), 0o644);
    assert_eq!(PRIVATE_FILE_MODE.bits(), 0o600);
}

#[test]
fn the_status_directory_holds_no_lock_or_private_record() {
    let directory = TIME_DIR.to_bytes();
    let within = |path: &CStr| {
        path.to_bytes()
            .strip_prefix(directory)
            .is_some_and(|rest| rest.starts_with(b"/"))
    };
    for path in [
        STATE_PATH,
        STATE_TEMP_PATH,
        DAEMON_PID_PATH,
        DAEMON_PID_TEMP_PATH,
    ] {
        assert!(within(path), "{path:?}");
    }
    for path in [
        STATE_LOCK_PATH,
        SUPPRESSION_PATH,
        SUPPRESSION_LOCK_PATH,
        RESTORE_PATH,
        RESTORE_TEMP_PATH,
        PORTB_LOCK_PATH,
    ] {
        assert!(!within(path), "{path:?}");
        assert!(path.to_bytes().starts_with(b"/run/nvx/"), "{path:?}");
    }
}

/// Stall detectors in a temporary directory, as files that hold their settings, and the
/// record and the lock of their suppression beside them.
struct TestDetectors {
    /// The temporary directory, which holds the files until the test ends.
    _directory: tempfile::TempDir,
    /// The detectors' files: RCU's stall warnings, the soft-lockup detector, and the
    /// hung-task detector.
    paths: [CString; 3],
    /// The suppression record.
    record: CString,
    /// The suppression lock.
    lock: CString,
}

impl TestDetectors {
    /// Returns detectors whose files hold `settings`, and are missing for `None`.
    fn new(settings: [Option<&str>; 3]) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = |name: &str| {
            CString::new(directory.path().join(name).into_os_string().into_vec()).unwrap()
        };
        let paths = ["rcu", "soft-lockup", "hung-task"].map(&path);
        for (path, setting) in paths.iter().zip(settings) {
            if let Some(setting) = setting {
                std::fs::write(std_path(path), format!("{setting}\n")).unwrap();
            }
        }
        let record = path("suppression");
        let lock = path("suppression.lock");
        Self {
            _directory: directory,
            paths,
            record,
            lock,
        }
    }

    /// Returns the detectors, with the value that suppresses each, as the guest's.
    fn detectors(&self) -> [(&CStr, i64); 3] {
        let [rcu, soft_lockup, hung_task] = &self.paths;
        [
            (rcu.as_c_str(), 1),
            (soft_lockup.as_c_str(), 0),
            (hung_task.as_c_str(), 0),
        ]
    }

    /// Returns the suppression of `detectors`, with the record and the lock.
    fn suppression<'a>(&'a self, detectors: &'a [(&'a CStr, i64)]) -> Suppression<'a> {
        Suppression {
            detectors,
            record: &self.record,
            lock: &self.lock,
        }
    }

    /// Returns the first line of each detector's file, or `None` for one that can't be read.
    fn settings(&self) -> [Option<String>; 3] {
        self.paths.each_ref().map(|path| {
            let text = sys::read_file(path).ok()?;
            let text = String::from_utf8(text).unwrap();
            Some(text.lines().next().unwrap_or_default().to_owned())
        })
    }
}

/// Returns `path` as a standard path.
fn std_path(path: &CStr) -> &Path {
    Path::new(OsStr::from_bytes(path.to_bytes()))
}

/// Returns `settings` as the detectors' settings that [`TestDetectors::settings`] returns.
fn settings(settings: [Option<&str>; 3]) -> [Option<String>; 3] {
    settings.map(|setting| setting.map(str::to_owned))
}

#[test]
fn a_suppression_saves_the_settings_and_suppresses_the_detectors_until_its_restore() {
    let test = TestDetectors::new([Some("0"), Some("60"), Some("120")]);
    let detectors = test.detectors();
    let suppression = test.suppression(&detectors);
    suppression.save().unwrap();
    let suppressed = settings([Some("1"), Some("0"), Some("0")]);
    assert_eq!(test.settings(), suppressed);
    let [rcu, soft_lockup, hung_task] = test.paths.each_ref().map(|path| path.to_str().unwrap());
    let record = format!("{rcu}=0\n{soft_lockup}=60\n{hung_task}=120\n");
    assert_eq!(sys::read_file(&test.record).unwrap(), record.as_bytes());

    // Another save finds the pending capture's record, and changes nothing.
    assert_eq!(suppression.save(), Err(SaveError::Undone(Errno::EEXIST)));
    assert_eq!(test.settings(), suppressed);
    assert_eq!(sys::read_file(&test.record).unwrap(), record.as_bytes());

    suppression.restore(false).unwrap();
    assert_eq!(
        test.settings(),
        settings([Some("0"), Some("60"), Some("120")])
    );
    assert_eq!(sys::check_exists(&test.record), Err(Errno::ENOENT));
    // Without a record, only a restore that allows a missing one succeeds.
    assert_eq!(
        suppression.restore(false).unwrap_err(),
        format!("read /run/nvx/time-suppression: {}", Errno::ENOENT.desc()).into_bytes()
    );
    suppression.restore(true).unwrap();
}

#[test]
fn a_save_that_fails_before_its_record_changes_nothing() {
    // The first detector, which must exist, is missing.
    let test = TestDetectors::new([None, Some("60"), Some("120")]);
    let detectors = test.detectors();
    let suppression = test.suppression(&detectors);
    let unchanged = settings([None, Some("60"), Some("120")]);
    assert_eq!(suppression.save(), Err(SaveError::Undone(Errno::ENOENT)));
    assert_eq!(sys::check_exists(&test.record), Err(Errno::ENOENT));
    assert_eq!(test.settings(), unchanged);

    // A detector that can't be read, while another capture is pending, leaves that capture's
    // record and the settings as they were.
    std::fs::create_dir(std_path(&test.paths[0])).unwrap();
    let pending = b"/proc/sys/kernel/hung_task_timeout_secs=120\n";
    std::fs::write(std_path(&test.record), pending).unwrap();
    let error = suppression.save().unwrap_err();
    assert!(matches!(error, SaveError::Undone(_)), "{error:?}");
    assert_eq!(sys::read_file(&test.record).unwrap(), pending);
    assert_eq!(test.settings(), unchanged);
}

/// Returns a save's write of a detector's setting, which fails with `EIO` for each detector
/// and value of `failing`, and writes the others.
fn failing_writes<'a>(
    failing: &'a [(&'a CStr, i64)],
) -> impl FnMut(&CStr, i64) -> Result<(), Errno> + 'a {
    move |path, value| {
        let fails = failing
            .iter()
            .any(|&(failing, failing_value)| failing == path && failing_value == value);
        if fails {
            Err(Errno::EIO)
        } else {
            sys::write_number(path, value)
        }
    }
}

#[test]
fn a_save_that_fails_to_suppress_a_detector_undoes_its_writes_and_removes_its_record() {
    let test = TestDetectors::new([Some("0"), Some("60"), Some("120")]);
    let detectors = test.detectors();
    let suppression = test.suppression(&detectors);
    let failing = [(test.paths[1].as_c_str(), 0)];
    assert_eq!(
        suppression.save_with(failing_writes(&failing)),
        Err(SaveError::Undone(Errno::EIO))
    );
    assert_eq!(
        test.settings(),
        settings([Some("0"), Some("60"), Some("120")])
    );
    assert_eq!(sys::check_exists(&test.record), Err(Errno::ENOENT));
}

#[test]
fn a_save_that_fails_to_undo_a_write_keeps_its_record_for_a_restore() {
    let test = TestDetectors::new([Some("0"), Some("60"), Some("120")]);
    let detectors = test.detectors();
    let suppression = test.suppression(&detectors);
    // The soft-lockup detector can't be suppressed, nor RCU's stall warnings restored.
    let failing = [(test.paths[1].as_c_str(), 0), (test.paths[0].as_c_str(), 0)];
    assert_eq!(
        suppression.save_with(failing_writes(&failing)),
        Err(SaveError::Kept(Errno::EIO))
    );
    assert_eq!(
        test.settings(),
        settings([Some("1"), Some("60"), Some("120")])
    );
    // The record keeps the settings, which a restore restores.
    suppression.restore(false).unwrap();
    assert_eq!(
        test.settings(),
        settings([Some("0"), Some("60"), Some("120")])
    );
    assert_eq!(sys::check_exists(&test.record), Err(Errno::ENOENT));
}

#[test]
fn a_save_waits_for_the_suppression_lock() {
    let test = TestDetectors::new([Some("0"), Some("60"), Some("120")]);
    let detectors = test.detectors();
    let suppression = test.suppression(&detectors);
    let lock = sys::lock_file(&test.lock, PRIVATE_FILE_MODE).unwrap();
    let (sender, receiver) = mpsc::channel();
    thread::scope(|scope| {
        scope.spawn(|| sender.send(suppression.save()).unwrap());
        // While a restore holds the lock, the save reads and changes nothing.
        assert_eq!(
            receiver.recv_timeout(Duration::from_millis(200)),
            Err(RecvTimeoutError::Timeout)
        );
        assert_eq!(
            test.settings(),
            settings([Some("0"), Some("60"), Some("120")])
        );
        drop(lock);
        assert_eq!(receiver.recv().unwrap(), Ok(()));
    });
    assert_eq!(test.settings(), settings([Some("1"), Some("0"), Some("0")]));
}
