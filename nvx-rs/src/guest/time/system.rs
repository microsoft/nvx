//! The time tools' runtime: persistent state, violation events, port-B transactions, and the
//! wall clock's discipline, over the system layer's owners.

use std::ffi::{CStr, CString};
use std::fmt::{self, Display};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use super::Phase;
use super::api::Violation;
use super::decoders::{
    self, ClockRates, GuestReads, HostClock, SAMPLE_SIZE, TimeSample, TimeSampleError,
};
use super::events::{EventSource, ViolationCode, format_event};
use super::state::{SampleRejection, TimeState};
use crate::guest::sys::x86::{self, msr};
use crate::guest::sys::{
    self, ClockAdjustment, ClockDiscipline, CreatedFile, Errno, FileDescriptor, FileLock,
    ForkResult, FrequencyHold, Mode, MsrAccess, MsrFile, OpenFlags, Port, PortAccess, PortError,
    TimeValLike, kernel,
};
use crate::protocol::status;

/// Statuses with which a fatal time-ABI failure stops the VM through the control port, which
/// `nvx::protocol::status` publishes for the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum PowerOffStatus {
    /// A time-ABI conformance check failed.
    Conformance = status::TIME_CONFORMANCE,
    /// The runtime watcher observed a time-ABI violation.
    Violation = status::TIME_VIOLATION,
    /// Restore-time repair could not complete safely.
    Repair = status::TIME_REPAIR,
}

impl PowerOffStatus {
    /// Returns the exact byte written to the control port.
    pub(crate) const fn code(self) -> u8 {
        self as u8
    }
}

/// Selector requesting the restore packet.
pub(crate) const SELECT_PACKET: u8 = 0xa5;
/// Selector requesting the VM generation identifier.
pub(crate) const SELECT_GENERATION_ID: u8 = 0xa6;
/// Selector requesting a host time sample.
pub(crate) const SELECT_TIME_SAMPLE: u8 = 0xa7;
/// Status bit indicating that a restore packet can be read.
pub(crate) const PORTB_PACKET_AVAILABLE: u8 = 0x02;
/// Status bit indicating that a generation identifier can be read.
pub(crate) const PORTB_GENERATION_ID_AVAILABLE: u8 = 0x20;
/// Status bit indicating that the time-sample window is supported.
pub(crate) const PORTB_TIME_WINDOW: u8 = 0x40;
/// Snapshot-port value acknowledging completed restore repair.
pub(crate) const SNAPSHOT_ACKNOWLEDGE: u8 = 2;

/// Top-level NVX runtime directory, which also holds the time tools' locks and private records.
pub(crate) const RUN_DIR: &CStr = c"/run/nvx";
/// Time-status directory, which holds only what any process may read: the published state and
/// the daemon's process identifier, and their temporary files. A runtime may bind it into a
/// workload's root, where even a workload of user ID 0 must reach no lock that it could hold,
/// so the locks and private records stay in [`RUN_DIR`].
pub(crate) const TIME_DIR: &CStr = c"/run/nvx/time";
/// Published time state file.
pub(crate) const STATE_PATH: &CStr = c"/run/nvx/time/state";
/// Temporary state file atomically renamed into place.
const STATE_TEMP_PATH: &CStr = c"/run/nvx/time/.state";
/// Advisory lock protecting state updates.
const STATE_LOCK_PATH: &CStr = c"/run/nvx/time-state.lock";
/// Published time-daemon process identifier.
pub(crate) const DAEMON_PID_PATH: &CStr = c"/run/nvx/time/daemon.pid";
/// Temporary daemon-PID file.
pub(crate) const DAEMON_PID_TEMP_PATH: &CStr = c"/run/nvx/time/daemon.pid.tmp";
/// The time daemon's command name, which it takes whichever program started it, and by which
/// the published process identifier is recognized.
pub(crate) const DAEMON_NAME: &CStr = c"nvx-time";
/// Saved stall-detector settings during capture/restore.
pub(crate) const SUPPRESSION_PATH: &CStr = c"/run/nvx/time-suppression";
/// Advisory lock protecting suppression state.
const SUPPRESSION_LOCK_PATH: &CStr = c"/run/nvx/time-suppression.lock";
/// Private restore record consumed by the daemon.
pub(crate) const RESTORE_PATH: &CStr = c"/run/nvx/time-restore";
/// Temporary restore-record file.
pub(crate) const RESTORE_TEMP_PATH: &CStr = c"/run/nvx/time-restore.tmp";
/// Advisory lock serializing port-window transactions.
pub(crate) const PORTB_LOCK_PATH: &CStr = c"/run/nvx/portb.lock";
/// Temporary mount point used to force an RCU grace period.
pub(crate) const RCU_SYNC_PATH: &CStr = c"/run/nvx/rcu-sync";

/// Mode of the runtime directories: `rwxr-xr-x`.
const DIRECTORY_MODE: Mode = Mode::S_IRWXU
    .union(Mode::S_IRGRP)
    .union(Mode::S_IXGRP)
    .union(Mode::S_IROTH)
    .union(Mode::S_IXOTH);
/// Mode of the files that other users may read, such as the state file: `rw-r--r--`.
const PUBLIC_FILE_MODE: Mode = Mode::S_IRUSR
    .union(Mode::S_IWUSR)
    .union(Mode::S_IRGRP)
    .union(Mode::S_IROTH);
/// Mode of the lock files, the suppression record, and the restored entropy: `rw-------`.
pub(crate) const PRIVATE_FILE_MODE: Mode = Mode::S_IRUSR.union(Mode::S_IWUSR);

/// Longest prefix of a path that a restore diagnostic quotes, as C's `%.200s` did.
const QUOTED_PATH_LIMIT: usize = 200;

/// The stall detectors that capture suppresses, and the value that suppresses each: RCU's stall
/// warnings, the soft-lockup detector, and the hung-task detector. Only the first must exist.
const SUPPRESSED_DETECTORS: [(&CStr, i64); 3] = [
    (kernel::RCU_STALL_SUPPRESS, 1),
    (kernel::SOFT_WATCHDOG, 0),
    (kernel::HUNG_TASK_TIMEOUT, 0),
];

/// The priority prefix of an event that the program writes to the kernel log: `KERN_CRIT`.
const KERNEL_LOG_PRIORITY: &[u8] = b"<2>";
/// The priority prefix of a diagnostic that the program writes to the kernel log: `KERN_ERR`.
const DIAGNOSTIC_PRIORITY: &[u8] = b"<3>";

/// Number of host samples that boot, restore repair, and each discipline poll take at most.
pub(crate) const SAMPLE_ATTEMPTS: usize = 3;
/// The largest uncertainty of a time sample that the guest accepts, in nanoseconds: the host
/// pairs its UTC with the guest TSC within 100 µs.
pub(crate) const SAMPLE_BOUND_NS: i64 = 100_000;
/// The smallest declared TSC rate, in Hz, that pairs a time sample: any lower rate is a misread
/// of the frequency MSR.
const MIN_TSC_HZ: u64 = 1_000_000;

/// Seconds after which RCU reports a CPU stall in NVX's guest kernels, which the checks require
/// and the daemon assumes when it cannot read the setting.
pub(crate) const DEFAULT_STALL_TIMEOUT_S: i64 = 21;

/// The privileged operations of the time tools, and the clock reads with which the guest pairs a
/// time sample, which a fake replaces in tests, so that their retry and failure paths are
/// testable.
pub(crate) trait PrivilegedOps: Sync {
    /// Reads the time-stamp counter after the instructions before it.
    fn read_tsc(&self) -> u64;
    /// Reads the realtime clock, in nanoseconds since the Unix epoch.
    fn realtime_ns(&self) -> i64;
    /// Takes the calling thread's permission for the time ABI's ports, for one transaction,
    /// which [`PortAccess::release`] ends.
    fn open_ports(&self) -> Result<PortAccess<'_>, PortError>;
    /// Opens the MSR device of `cpu`.
    fn open_msr(&self, cpu: usize, access: MsrAccess) -> Result<MsrDescriptor, Errno>;
    /// Reads the register `msr`.
    fn read_msr(&self, descriptor: &MsrDescriptor, msr: u32) -> Result<u64, Errno>;
    /// Writes `value` to the register `msr`.
    fn write_msr(&self, descriptor: &MsrDescriptor, msr: u32, value: u64) -> Result<(), Errno>;
    /// Applies `adjustment` to the wall clock's discipline.
    fn adjust_clock(&self, adjustment: ClockAdjustment) -> Result<ClockDiscipline, Errno>;
    /// Stops the VM with `status`, which in production does not return.
    fn power_off(&self, status: PowerOffStatus);
    /// Appends `record` to the kernel log, when the process may write it.
    fn write_kernel_log(&self, record: &[u8]);
    /// Writes `separator` and then `line` to the console, or to standard error when the console
    /// does not open.
    fn write_console(&self, separator: &[u8], line: &[u8]);
}

/// An MSR device, or the token of a test's fake.
pub(crate) enum MsrDescriptor {
    /// A device that the kernel opened.
    Real(MsrFile),
    /// A test's token, which owns no descriptor.
    #[cfg(test)]
    Fake,
}

/// The production privileged operations.
///
/// They take the port permission for each transaction, on the thread that runs it, rather than
/// for the whole command: Linux grants it per thread, and the threads and processes that a
/// holder starts inherit it.
pub(crate) struct RealOps;

impl PrivilegedOps for RealOps {
    fn read_tsc(&self) -> u64 {
        x86::rdtsc_lfence()
    }

    fn realtime_ns(&self) -> i64 {
        clock_ns(sys::Clock::Realtime)
    }

    fn open_ports(&self) -> Result<PortAccess<'_>, PortError> {
        PortAccess::request()
    }

    fn open_msr(&self, cpu: usize, access: MsrAccess) -> Result<MsrDescriptor, Errno> {
        MsrFile::open(cpu, access).map(MsrDescriptor::Real)
    }

    fn read_msr(&self, descriptor: &MsrDescriptor, msr: u32) -> Result<u64, Errno> {
        match descriptor {
            MsrDescriptor::Real(file) => file.read(msr),
            #[cfg(test)]
            MsrDescriptor::Fake => Err(Errno::EIO),
        }
    }

    fn write_msr(&self, descriptor: &MsrDescriptor, msr: u32, value: u64) -> Result<(), Errno> {
        match descriptor {
            MsrDescriptor::Real(file) => file.write(msr, value),
            #[cfg(test)]
            MsrDescriptor::Fake => Err(Errno::EIO),
        }
    }

    fn adjust_clock(&self, adjustment: ClockAdjustment) -> Result<ClockDiscipline, Errno> {
        sys::adjust_clock(adjustment)
    }

    fn power_off(&self, status: PowerOffStatus) {
        sys::power_off(status.code())
    }

    fn write_kernel_log(&self, record: &[u8]) {
        let flags = OpenFlags::O_WRONLY | OpenFlags::O_CLOEXEC;
        if let Ok(log) = sys::open(kernel::KERNEL_LOG_DEVICE, flags, Mode::empty()) {
            let _ = log.write_all(record);
        }
    }

    fn write_console(&self, separator: &[u8], line: &[u8]) {
        let flags = OpenFlags::O_WRONLY | OpenFlags::O_NOCTTY | OpenFlags::O_CLOEXEC;
        match sys::open(kernel::CONSOLE_DEVICE, flags, Mode::empty()) {
            Ok(console) => {
                let _ = console.write_all(separator);
                let _ = console.write_all(line);
            }
            Err(_) => {
                sys::write_standard_error(separator);
                sys::write_standard_error(line);
            }
        }
    }
}

/// Returns the negative `errno` of an MSR access, or zero for one that succeeded, as the time
/// ABI's diagnostics print it.
pub(crate) fn msr_error<T>(result: &Result<T, Errno>) -> i32 {
    result.as_ref().err().map_or(0, |&error| -(error as i32))
}

/// Shared runtime state and injectable privileged operations.
pub(crate) struct Runtime<'a> {
    /// Privileged-operation implementation.
    pub(crate) ops: &'a dyn PrivilegedOps,
    /// Whether violations are reported instead of powering off the VM.
    report_only: bool,
    /// Whether a violation that powers off the VM in enforcing mode fails the call instead, which
    /// the runtime keeps the violation for.
    strict: bool,
    /// The first violation that the call reported in strict mode, which fails the call.
    violation: Mutex<Option<Violation>>,
    /// Current VM generation.
    generation: AtomicU32,
    /// The declared TSC rate, `F`, in Hz, once known, or zero.
    tsc_hz: AtomicU64,
    /// Whether an asynchronous child may have interrupted console output.
    async_output: AtomicBool,
    /// Serializes multi-destination violation records.
    report_lock: Mutex<()>,
}

impl<'a> Runtime<'a> {
    /// Creates runtime state around one privileged-operation implementation.
    pub(crate) fn new(ops: &'a dyn PrivilegedOps, report_only: bool) -> Self {
        Self {
            ops,
            report_only,
            strict: false,
            violation: Mutex::new(None),
            generation: AtomicU32::new(0),
            tsc_hz: AtomicU64::new(0),
            async_output: AtomicBool::new(false),
            report_lock: Mutex::new(()),
        }
    }

    /// Returns the runtime in strict mode, unless it is report-only: a violation that powers off
    /// the VM in enforcing mode is reported by its event alone, and the runtime keeps the first
    /// such violation, with which the call fails, as [`take_violation`](Self::take_violation)
    /// tells. The runtime then never powers off the VM.
    pub(crate) fn strict(mut self) -> Self {
        self.strict = !self.report_only;
        self
    }

    /// Returns whether conformance failures are report-only.
    pub(crate) fn report_only(&self) -> bool {
        self.report_only
    }

    /// Returns whether a violation fails the call instead of powering off the VM.
    pub(crate) fn is_strict(&self) -> bool {
        self.strict
    }

    /// Takes the first violation that the call reported in strict mode, which fails the call,
    /// if any.
    pub(crate) fn take_violation(&self) -> Option<Violation> {
        self.violation
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// Returns the current VM generation.
    pub(crate) fn generation(&self) -> u32 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Publishes the current VM generation to concurrent checks.
    pub(crate) fn set_generation(&self, value: u32) {
        self.generation.store(value, Ordering::Relaxed);
    }

    /// Records the declared TSC rate, `F`, in Hz, that the boot checks validated, unless it is
    /// zero, for none.
    pub(crate) fn set_tsc_hz(&self, value: u64) {
        if value != 0 {
            self.tsc_hz.store(value, Ordering::Relaxed);
        }
    }

    /// Returns the declared TSC rate, `F`, in Hz: the one that the runtime recorded, or else the
    /// frequency MSR's, of CPU 0, which it then records.
    pub(crate) fn tsc_hz(&self) -> Result<u64, SampleError> {
        let recorded = self.tsc_hz.load(Ordering::Relaxed);
        if recorded != 0 {
            return Ok(recorded);
        }
        let read = self
            .ops
            .open_msr(0, MsrAccess::Read)
            .and_then(|descriptor| self.ops.read_msr(&descriptor, msr::HV_TSC_FREQUENCY));
        match read {
            Ok(rate) if rate >= MIN_TSC_HZ => {
                self.tsc_hz.store(rate, Ordering::Relaxed);
                Ok(rate)
            }
            read => Err(SampleError::TscRate(read)),
        }
    }

    /// Marks whether console output may need a leading newline.
    pub(crate) fn set_async_output(&self, value: bool) {
        self.async_output.store(value, Ordering::Relaxed);
    }

    /// Takes the calling thread's permission for the time ABI's ports, for one transaction,
    /// which [`PortAccess::release`] ends.
    pub(crate) fn open_ports(&self) -> Result<PortAccess<'a>, PortError> {
        self.ops.open_ports()
    }

    /// Writes `error`, the failure of a transaction's end, to the kernel log, such as
    /// `nvx-time: ioperm revoke: Cannot allocate memory`, where nothing else reports it: after an
    /// event's write to OpenVMM's debug console, and in the time daemon, whose standard streams
    /// are on the null device. Until a later transaction revokes the permission, the thread
    /// starts nothing, as [`fork_process`] checks.
    pub(crate) fn log_release_failure(&self, error: PortError) {
        let record = format!("nvx-time: {error}\n");
        self.ops
            .write_kernel_log(&[DIAGNOSTIC_PRIORITY, record.as_bytes()].concat());
    }

    /// Emits one serialized violation record to every ABI destination.
    pub(crate) fn emit_event(
        &self,
        code: ViolationCode,
        source: EventSource,
        phase: Phase,
        detail: &[u8],
    ) {
        let _guard = self.report_lock.lock().ok();
        self.emit_event_unlocked(code, source, phase, detail);
    }

    /// Emits an event while the caller already owns `report_lock`.
    fn emit_event_unlocked(
        &self,
        code: ViolationCode,
        source: EventSource,
        phase: Phase,
        detail: &[u8],
    ) {
        let line = format_event(
            self.report_only(),
            code,
            source,
            phase,
            self.generation(),
            clock_ns(sys::Clock::Boottime),
            detail,
        );
        self.ops
            .write_kernel_log(&[KERNEL_LOG_PRIORITY, line.as_bytes()].concat());
        if let Ok(ports) = self.open_ports() {
            if self.async_output.load(Ordering::Relaxed) {
                ports.write_u8(Port::PortbData, b'\n');
            }
            for byte in line.bytes() {
                ports.write_u8(Port::PortbData, byte);
            }
            // The event goes on after a failure, which keeps the thread from starting anything,
            // as `fork_process` checks.
            if let Err(error) = ports.release() {
                self.log_release_failure(error);
            }
        }
        let separator: &[u8] = if self.async_output.load(Ordering::Relaxed) {
            b"\n"
        } else {
            b""
        };
        self.ops.write_console(separator, line.as_bytes());
        let _ = count_violation();
    }

    /// Reports a conformance failure, and powers off unless report-only or strict.
    pub(crate) fn conformance_failure(&self, code: ViolationCode, phase: Phase, detail: &[u8]) {
        let _guard = self.report_lock.lock().ok();
        self.emit_event_unlocked(code, EventSource::Conformance, phase, detail);
        self.stop(PowerOffStatus::Conformance, code, detail);
    }

    /// Emits a fatal event and requests its typed power-off status, unless report-only or
    /// strict.
    pub(crate) fn fail_fatal(
        &self,
        status: PowerOffStatus,
        code: ViolationCode,
        source: EventSource,
        phase: Phase,
        detail: &[u8],
    ) {
        self.emit_event(code, source, phase, detail);
        self.stop(status, code, detail);
    }

    /// Reports that restore-time repair failed, a fatal event of the restore phase, and powers off
    /// with the repair status unless report-only or strict.
    pub(crate) fn repair_failure(&self, code: ViolationCode, detail: &[u8]) {
        self.fail_fatal(
            PowerOffStatus::Repair,
            code,
            EventSource::Repair,
            Phase::Restore,
            detail,
        );
    }

    /// Stops the VM with `status` once its event reported the violation of `code`, with `detail`:
    /// powers it off in enforcing mode, keeps the call's first violation in strict mode, and does
    /// neither in report-only mode.
    fn stop(&self, status: PowerOffStatus, code: ViolationCode, detail: &[u8]) {
        if self.is_strict() {
            self.violation
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get_or_insert_with(|| Violation::new(status, code, detail));
        } else if !self.report_only {
            self.ops.power_off(status);
        }
    }
}

/// Reads one Linux clock in nanoseconds, or zero when it cannot be read.
pub(crate) fn clock_ns(clock: sys::Clock) -> i64 {
    sys::clock_time(clock).map_or(0, |time| time.num_nanoseconds())
}

/// Returns process-wide CPU time in wrapping nanoseconds.
pub(crate) fn cpu_time_ns() -> i64 {
    clock_ns(sys::Clock::ProcessCpuTime)
}

/// Why the time tools could not start a process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpawnError {
    /// The calling thread may hold port permission, which the process would inherit.
    Ports(PortError),
    /// `fork` failed.
    Fork(Errno),
}

impl Display for SpawnError {
    /// Writes the diagnostic: the description of `fork`'s `errno`, or why the thread may hold
    /// port permission, such as `ioperm revoke: Cannot allocate memory`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Ports(error) => error.fmt(formatter),
            Self::Fork(error) => formatter.write_str(error.desc()),
        }
    }
}

impl std::error::Error for SpawnError {}

/// Forks the process, as [`sys::fork`] does, once the calling thread holds no port permission,
/// which the child would inherit.
pub(crate) fn fork_process() -> Result<ForkResult, SpawnError> {
    sys::check_released().map_err(SpawnError::Ports)?;
    sys::fork().map_err(SpawnError::Fork)
}

/// Creates the NVX runtime directories, ignoring preexistence and failures.
pub(crate) fn make_runtime_directories() {
    let _ = sys::create_directory(RUN_DIR, DIRECTORY_MODE);
    let _ = sys::create_directory(TIME_DIR, DIRECTORY_MODE);
}

/// Durably writes `text` to `temporary`, and atomically renames it to `path`. A failure after
/// the file was created removes it.
pub(crate) fn replace_file(path: &CStr, temporary: &CStr, text: &[u8]) -> Result<(), Errno> {
    let file = CreatedFile::create(temporary, false, PUBLIC_FILE_MODE)?;
    file.write_all(text)?;
    file.sync()?;
    file.rename(path)
}

/// Opens or creates a runtime lock file and takes its exclusive advisory lock.
pub(crate) fn lock_file(path: &CStr) -> Result<FileLock, Errno> {
    make_runtime_directories();
    sys::lock_file(path, PRIVATE_FILE_MODE)
}

/// Loads and parses the published time state.
pub(crate) fn state_load() -> Result<TimeState, Errno> {
    let text = sys::read_file(STATE_PATH)?;
    TimeState::parse(&text).ok_or(Errno::EINVAL)
}

/// Atomically persists one complete time state.
pub(crate) fn state_store(state: &TimeState) -> Result<(), Errno> {
    replace_file(STATE_PATH, STATE_TEMP_PATH, state.to_string().as_bytes())
}

/// Applies one read-modify-write update to the time state under its lock.
pub(crate) fn state_apply(update: impl FnOnce(&mut TimeState)) -> Result<(), Errno> {
    let _lock = lock_file(STATE_LOCK_PATH)?;
    let mut state = state_load().unwrap_or_default();
    update(&mut state);
    state_store(&state)
}

/// Increments the persisted violation counter when state exists.
fn count_violation() -> Result<(), Errno> {
    if !sys::path_exists(STATE_PATH) {
        return Ok(());
    }
    state_apply(|state| state.violations = state.violations.wrapping_add(1))
}

/// Loads validated TSC and LAPIC rates from state.
pub(crate) fn rates_load() -> Result<(u64, u64), Errno> {
    let state = state_load()?;
    if state.tsc_hz == 0 || state.lapic_hz == 0 {
        Err(Errno::EINVAL)
    } else {
        Ok((state.tsc_hz, state.lapic_hz))
    }
}

/// Saves the stall detectors' settings in the suppression record, and suppresses them for a
/// snapshot capture; see [`Suppression::save`]. A record that exists already, of a pending
/// capture, is `EEXIST`.
pub(crate) fn suppression_save() -> Result<(), SaveError> {
    make_runtime_directories();
    SUPPRESSION.save()
}

/// Why a save of the stall detectors' settings failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SaveError {
    /// The save failed with this error, and left the settings as they were: it changed none, or
    /// undid every change and removed its record. A record that exists already, of a pending
    /// capture, is `EEXIST`, and stays that capture's.
    Undone(Errno),
    /// The save failed to suppress a detector with this error, then to undo every change, so it
    /// kept its record, from which [`suppression_restore`] restores the settings.
    Kept(Errno),
}

/// Formats a diagnostic: `prefix`, then the description of `error`.
fn describe(prefix: &[u8], error: Errno) -> Vec<u8> {
    [prefix, error.desc().as_bytes()].concat()
}

/// Restores the stall detectors' settings from the suppression record, and removes the record;
/// see [`Suppression::restore`]. A missing record succeeds when `missing_ok`.
pub(crate) fn suppression_restore(missing_ok: bool) -> Result<(), Vec<u8>> {
    make_runtime_directories();
    SUPPRESSION.restore(missing_ok)
}

/// The suppression of the stall detectors for a capture: the detectors, the record of their
/// saved settings, which exists while a capture is pending, and the lock that serializes the
/// record's saves and restores, so that each restores only the settings that a save recorded.
struct Suppression<'a> {
    /// The detectors, of which only the first must exist, and the value that suppresses each.
    detectors: &'a [(&'a CStr, i64)],
    /// The record of the detectors' saved settings.
    record: &'a CStr,
    /// The lock that serializes the saves and restores of the record.
    lock: &'a CStr,
}

/// The guest's suppression of its stall detectors.
const SUPPRESSION: Suppression<'static> = Suppression {
    detectors: &SUPPRESSED_DETECTORS,
    record: SUPPRESSION_PATH,
    lock: SUPPRESSION_LOCK_PATH,
};

impl Suppression<'_> {
    /// Saves the detectors' settings in the record, and suppresses the detectors, under the
    /// lock, so that no restore runs between its reads and its writes. A record that exists
    /// already, of a pending capture, is `EEXIST`. A save that fails before it created the
    /// record changes nothing; one that fails after undoes its changes and removes the record,
    /// or keeps the record if it can't.
    fn save(&self) -> Result<(), SaveError> {
        self.save_with(sys::write_number)
    }

    /// Implements [`save`](Self::save), with `write` writing a detector's setting.
    fn save_with(
        &self,
        mut write: impl FnMut(&CStr, i64) -> Result<(), Errno>,
    ) -> Result<(), SaveError> {
        let _lock = sys::lock_file(self.lock, PRIVATE_FILE_MODE).map_err(SaveError::Undone)?;
        let mut saved = Vec::new();
        for (index, &(path, suppressed)) in self.detectors.iter().enumerate() {
            if let Err(error) = sys::check_exists(path) {
                if index == 0 {
                    return Err(SaveError::Undone(error));
                }
                continue;
            }
            let value = sys::read_number::<i64>(path).map_err(SaveError::Undone)?;
            saved.push((path, value, suppressed));
        }
        let mut text = Vec::new();
        for (path, value, _) in &saved {
            text.extend_from_slice(path.to_bytes());
            text.extend_from_slice(format!("={value}\n").as_bytes());
        }
        let record =
            CreatedFile::create(self.record, true, PRIVATE_FILE_MODE).map_err(SaveError::Undone)?;
        record.write_all(&text).map_err(SaveError::Undone)?;
        // As in the C program, a record that fails to close stays, and the suppression goes on.
        let _ = record.keep();
        for (written, &(path, _, suppressed)) in saved.iter().enumerate() {
            if let Err(error) = write(path, suppressed) {
                // The record stays unless every earlier write is undone, so that a restore can
                // still restore the settings.
                let mut undone = true;
                for &(path, value, _) in &saved[..written] {
                    undone &= write(path, value).is_ok();
                }
                if undone && sys::unlink(self.record).is_ok() {
                    return Err(SaveError::Undone(error));
                }
                return Err(SaveError::Kept(error));
            }
        }
        Ok(())
    }

    /// Restores the detectors' settings from the record, and removes it, under the lock. A
    /// missing record succeeds when `missing_ok`.
    fn restore(&self, missing_ok: bool) -> Result<(), Vec<u8>> {
        let _lock = sys::lock_file(self.lock, PRIVATE_FILE_MODE)
            .map_err(|error| describe(b"lock /run/nvx/time-suppression.lock: ", error))?;
        let text = match sys::read_file(self.record) {
            Ok(text) => text,
            Err(Errno::ENOENT) if missing_ok => return Ok(()),
            Err(error) => return Err(describe(b"read /run/nvx/time-suppression: ", error)),
        };
        for line in text
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let Some(equal) = line.iter().position(|byte| *byte == b'=') else {
                break;
            };
            let Ok(path) = CString::new(&line[..equal]) else {
                break;
            };
            if let Err(error) = sys::write_file(&path, &line[equal + 1..]) {
                let quoted = &line[..equal.min(QUOTED_PATH_LIMIT)];
                return Err(describe(
                    &[b"restore ".as_slice(), quoted, b": "].concat(),
                    error,
                ));
            }
        }
        sys::unlink(self.record)
            .map_err(|error| describe(b"remove /run/nvx/time-suppression: ", error))
    }
}

/// Reads a byte buffer using one scalar four-byte port access per chunk.
pub(crate) fn portb_read(ports: &PortAccess<'_>, port: Port, buffer: &mut [u8]) {
    for chunk in buffer.chunks_mut(size_of::<u32>()) {
        let bytes = ports.read_u32(port).to_ne_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
}

/// Locks the shared port-window transaction.
pub(crate) fn lock_portb() -> Result<FileLock, Errno> {
    lock_file(PORTB_LOCK_PATH)
}

/// Formats a failure of [`lock_portb`]: `portb lock: `, then the description of `error`.
pub(crate) fn portb_lock_failure(error: Errno) -> Vec<u8> {
    describe(b"portb lock: ", error)
}

/// Latches and decodes one host time sample: brackets the selector write with TSC reads, then
/// reads the realtime clock between two more, with which the guest pairs the sample's guest TSC,
/// before it reads the time window.
pub(crate) fn take_sample(
    ports: &PortAccess<'_>,
    ops: &dyn PrivilegedOps,
) -> Result<(TimeSample, GuestReads), TimeSampleError> {
    let mut bytes = [0_u8; SAMPLE_SIZE];
    let before = ops.read_tsc();
    ports.write_u8(Port::PortbStatus, SELECT_TIME_SAMPLE);
    let after = ops.read_tsc();
    let realtime_ns = ops.realtime_ns();
    let end = ops.read_tsc();
    portb_read(ports, Port::PortbWindow, &mut bytes);
    let half_cycles = end.saturating_sub(after).div_ceil(2);
    let reads = GuestReads {
        before,
        after,
        realtime_ns,
        realtime_tsc: after.saturating_add(half_cycles),
        realtime_half_cycles: half_cycles,
    };
    Ok((TimeSample::parse(&bytes)?, reads))
}

/// Narrowest valid sample observed during one attempt group, and the frequency correction that
/// its host clock correction asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BestSample {
    /// Guest-to-host offset in nanoseconds.
    pub(crate) theta: i64,
    /// Pairing uncertainty in nanoseconds.
    pub(crate) epsilon: i64,
    /// The discipline of host UTC that the sample published.
    pub(crate) host: HostClock,
    /// The frequency correction, in scaled ppm, that runs the wall clock at host UTC's rate.
    pub(crate) frequency: i64,
    /// Whether Linux's ±500 ppm limit clamped `frequency`.
    pub(crate) limited: bool,
}

/// Whether the selected sample satisfied the requested uncertainty bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SampleResult {
    /// The sample was within the bound.
    Accepted,
    /// A valid sample was selected but remained wider than the bound.
    Wide,
}

/// Why no host time sample could be selected. Its `Display` is the diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SampleError {
    /// The thread could not take, or give up, the permission for the ports.
    Ports(PortError),
    /// The guest could not read the declared TSC rate, with which it pairs a sample, from the
    /// frequency MSR: the error, or the implausible rate that it read.
    TscRate(Result<u64, Errno>),
    /// The port window, of this status, offers no time sample.
    NoTimeWindow(u8),
    /// A sample came from another VM generation, which a restore must handle first.
    Generation {
        /// The sample's generation.
        sample: u32,
        /// The generation that the guest expected.
        expected: u32,
    },
    /// No attempt took a valid sample: the last attempt's error, if any.
    Invalid(Option<TimeSampleError>),
}

impl SampleError {
    /// Returns how the discipline records the rejection of a poll's sample that failed so.
    pub(crate) fn rejection(&self) -> SampleRejection {
        match self {
            Self::Invalid(Some(TimeSampleError::Tsc { .. })) => SampleRejection::Tsc,
            _ => SampleRejection::Invalid,
        }
    }
}

impl Display for SampleError {
    /// Writes the diagnostic, such as `time sample generation 7, expected 6`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Ports(error) => error.fmt(formatter),
            Self::TscRate(Err(error)) => write!(
                formatter,
                "MSR {:#010x} read: {}",
                msr::HV_TSC_FREQUENCY,
                error.desc()
            ),
            Self::TscRate(Ok(rate)) => write!(
                formatter,
                "MSR {:#010x} (TSC rate) is {rate} Hz",
                msr::HV_TSC_FREQUENCY
            ),
            Self::NoTimeWindow(status) => write!(
                formatter,
                "portb status 0x{status:02x} has no time-sample window"
            ),
            Self::Generation { sample, expected } => write!(
                formatter,
                "time sample generation {sample}, expected {expected}"
            ),
            Self::Invalid(Some(error)) => error.fmt(formatter),
            Self::Invalid(None) => formatter.write_str("no valid time sample"),
        }
    }
}

impl std::error::Error for SampleError {}

/// Returns the rates with which the guest's realtime clock counts its TSC: the declared TSC rate,
/// and the kernel's frequency correction.
pub(crate) fn clock_rates(runtime: &Runtime<'_>) -> Result<ClockRates, SampleError> {
    Ok(ClockRates {
        tsc_hz: runtime.tsc_hz()?,
        frequency: current_frequency(runtime),
    })
}

/// Selects the lowest-uncertainty valid sample from a bounded attempt count, in one port
/// transaction, which ends before this returns.
pub(crate) fn best_sample(
    runtime: &Runtime<'_>,
    attempts: usize,
    bound_ns: i64,
    generation: u32,
) -> Result<(SampleResult, BestSample), SampleError> {
    let rates = clock_rates(runtime)?;
    let ports = runtime.open_ports().map_err(SampleError::Ports)?;
    let selected = select_sample(&ports, runtime.ops, attempts, bound_ns, generation, rates);
    ports.release().map_err(SampleError::Ports)?;
    selected
}

/// Selects the lowest-uncertainty valid sample through `ports`, pairing each with the guest's
/// clocks at `rates`, as [`best_sample`] does.
pub(crate) fn select_sample(
    ports: &PortAccess<'_>,
    ops: &dyn PrivilegedOps,
    attempts: usize,
    bound_ns: i64,
    generation: u32,
    rates: ClockRates,
) -> Result<(SampleResult, BestSample), SampleError> {
    let status = ports.read_u8(Port::PortbStatus);
    if status & PORTB_TIME_WINDOW == 0 {
        return Err(SampleError::NoTimeWindow(status));
    }
    let mut best: Option<BestSample> = None;
    let mut invalid = None;
    for _ in 0..attempts {
        let (sample, reads) = match take_sample(ports, ops) {
            Ok(taken) => taken,
            Err(error) => {
                invalid = Some(error);
                continue;
            }
        };
        if sample.generation != generation {
            return Err(SampleError::Generation {
                sample: sample.generation,
                expected: generation,
            });
        }
        let (theta, epsilon) = match decoders::pair_sample(&sample, &reads, rates) {
            Ok(pairing) => pairing,
            Err(error) => {
                invalid = Some(error);
                continue;
            }
        };
        let (frequency, limited) = decoders::guest_frequency(sample.host.correction, rates.tsc_hz);
        let candidate = BestSample {
            theta,
            epsilon,
            host: sample.host,
            frequency,
            limited,
        };
        if best.is_none_or(|best| epsilon < best.epsilon) {
            best = Some(candidate);
        }
        if epsilon <= bound_ns {
            return Ok((SampleResult::Accepted, candidate));
        }
    }
    best.map(|best| (SampleResult::Wide, best))
        .ok_or(SampleError::Invalid(invalid))
}

/// Steps realtime by a signed nanosecond offset.
pub(crate) fn step_clock(runtime: &Runtime<'_>, offset_ns: i64) -> Result<(), Errno> {
    let (seconds, nanoseconds) = decoders::split_offset(offset_ns);
    runtime
        .ops
        .adjust_clock(ClockAdjustment::Step {
            seconds,
            nanoseconds,
        })
        .map(drop)
}

/// Holds Linux's scaled-ppm frequency correction, with its error estimates.
pub(crate) fn hold_frequency(runtime: &Runtime<'_>, hold: FrequencyHold) -> Result<(), Errno> {
    runtime
        .ops
        .adjust_clock(ClockAdjustment::Frequency(hold))
        .map(drop)
}

/// Queries the current scaled-ppm frequency, returning zero on failure.
pub(crate) fn current_frequency(runtime: &Runtime<'_>) -> i64 {
    runtime
        .ops
        .adjust_clock(ClockAdjustment::Query)
        .map_or(0, |discipline| discipline.scaled_ppm)
}

/// Steps the wall clock to host UTC by `sample`'s offset, and then holds the frequency
/// correction that its host clock correction asks for, with error estimates of its uncertainty
/// and the host's error.
pub(crate) fn synchronize(runtime: &Runtime<'_>, sample: &BestSample) -> Result<(), Errno> {
    step_clock(runtime, sample.theta)?;
    hold_frequency(
        runtime,
        decoders::frequency_hold(0, sample.epsilon, sample.frequency, sample.host.error_us()),
    )
}

/// Takes the initial sample within the bound, steps realtime to it, and holds the frequency
/// correction that the host's clock correction asks for.
pub(crate) fn initial_sample(runtime: &Runtime<'_>) -> Result<BestSample, Vec<u8>> {
    let (result, sample) = {
        let _lock = lock_portb().map_err(portb_lock_failure)?;
        best_sample(
            runtime,
            SAMPLE_ATTEMPTS,
            SAMPLE_BOUND_NS,
            runtime.generation(),
        )
        .map_err(|error| error.to_string().into_bytes())?
    };
    if result == SampleResult::Wide {
        return Err(wide_sample(sample.epsilon));
    }
    synchronize(runtime, &sample).map_err(|error| describe(b"adjtimex: ", error))?;
    Ok(sample)
}

/// Formats the diagnostic of a selected sample of the uncertainty `epsilon`, wider than the
/// bound.
pub(crate) fn wide_sample(epsilon: i64) -> Vec<u8> {
    format!("time sample uncertainty {epsilon} ns exceeds {SAMPLE_BOUND_NS} ns").into_bytes()
}

/// Writes `entropy` at the start of the restored entropy file, and reports a failure to write
/// or close it with the file's path.
pub(crate) fn write_entropy(
    file: FileDescriptor,
    path: &CStr,
    entropy: &[u8],
) -> Result<(), Vec<u8>> {
    let written = loop {
        match file.pwrite(entropy, 0) {
            Err(Errno::EINTR) => {}
            result => break result,
        }
    };
    let closed = file.close();
    let error = match (written, closed) {
        (Ok(count), Ok(())) if count == entropy.len() => return Ok(()),
        (Err(error), _) | (Ok(_), Err(error)) => error,
        (Ok(_), Ok(())) => Errno::EIO,
    };
    let path = &path.to_bytes()[..path.to_bytes().len().min(QUOTED_PATH_LIMIT)];
    Err(describe(
        &[b"write ".as_slice(), path, b": "].concat(),
        error,
    ))
}

#[cfg(test)]
pub(crate) mod tests {
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
    use crate::guest::sys::PortBackend;
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
        sample[..4].copy_from_slice(&[2, 0, host_status_byte(host.status), 0]);
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
    fn packet_without_ranges_costs_exactly_twenty_two_data_reads() {
        let ops = FakeOps::new(0..22);
        let ports = ops.open_ports().unwrap();
        let mut header = [0; decoders::PACKET_HEADER_SIZE];
        let mut entropy = [0; decoders::PACKET_ENTROPY_SIZE];
        portb_read(&ports, Port::PortbData, &mut header);
        portb_read(&ports, Port::PortbData, &mut entropy);
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
        let [rcu, soft_lockup, hung_task] =
            test.paths.each_ref().map(|path| path.to_str().unwrap());
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
}
